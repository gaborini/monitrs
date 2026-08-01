//! The bounded writer every widget in this crate draws through.
//!
//! §5.7 makes "never panic because a calculated rectangle has zero width or
//! height" a hard requirement, and this crate additionally promises that a widget
//! never writes outside its own `Rect`. Both promises are made *structurally*
//! rather than by review: a widget is handed a [`Painter`], the painter's origin
//! is its rectangle's origin, and every coordinate it accepts is relative to that
//! origin and clipped against it.
//!
//! [`ratatui::buffer::Buffer::set_stringn`] clips to the *buffer's* right edge,
//! which is not the same thing as the widget's rectangle: a widget rendered into
//! the left half of the screen could happily write across the right half. That is
//! the specific bug this type exists to make unrepresentable.

use monitrs_core::units::{display_width, pad_left, pad_right};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::glyphs::GlyphSet;
use crate::layout::Align;
use crate::widgets::states::fit_within;

/// A write-clipped view of one rectangle of a [`Buffer`].
///
/// Coordinates passed to every method are *relative* to [`Painter::area`], so a
/// widget can be written as though it always started at `(0, 0)`. Anything that
/// would land outside the rectangle is dropped, never wrapped and never
/// truncated into a neighbouring panel.
#[derive(Debug)]
pub struct Painter<'buf> {
    buffer: &'buf mut Buffer,
    area: Rect,
}

impl<'buf> Painter<'buf> {
    /// Binds a painter to `area` within `buffer`.
    ///
    /// `area` is intersected with the buffer's own area first, so an oversized or
    /// off-screen rectangle yields a painter that simply has less room rather
    /// than one that writes out of bounds.
    #[must_use]
    pub fn new(buffer: &'buf mut Buffer, area: Rect) -> Self {
        let area = buffer.area.intersection(area);
        Self { buffer, area }
    }

    /// The clipped rectangle this painter writes into.
    #[must_use]
    pub const fn area(&self) -> Rect {
        self.area
    }

    /// Usable width in cells.
    #[must_use]
    pub const fn width(&self) -> u16 {
        self.area.width
    }

    /// Usable height in rows.
    #[must_use]
    pub const fn height(&self) -> u16 {
        self.area.height
    }

    /// Whether there is nowhere to draw. Rendering into an empty painter is legal
    /// and does nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.area.is_empty()
    }

    /// Cells left on row `y` from relative column `x` onwards.
    ///
    /// Zero when `x` is past the right edge, so callers can budget with plain
    /// subtraction and never underflow.
    #[must_use]
    pub const fn remaining(&self, x: u16) -> u16 {
        self.area.width.saturating_sub(x)
    }

    /// A painter for a sub-rectangle, expressed relative to this one's origin.
    ///
    /// The result is clipped to the parent, so a child can never be given more
    /// room than its parent had.
    #[must_use]
    pub fn sub(&mut self, relative: Rect) -> Painter<'_> {
        let absolute = Rect {
            x: self.area.x.saturating_add(relative.x),
            y: self.area.y.saturating_add(relative.y),
            width: relative.width,
            height: relative.height,
        };
        Painter {
            buffer: self.buffer,
            area: self.area.intersection(absolute),
        }
    }

    /// A painter for one row, expressed relative to this one's origin.
    #[must_use]
    pub fn row(&mut self, y: u16) -> Painter<'_> {
        let width = self.area.width;
        self.sub(Rect {
            x: 0,
            y,
            width,
            height: 1,
        })
    }

    /// Writes `text` at `(x, y)`, clipped to the right edge, returning the number
    /// of cells consumed.
    ///
    /// A grapheme that would only partly fit is dropped rather than split, which
    /// is what keeps a double-width character from corrupting the column to its
    /// right.
    pub fn write(&mut self, x: u16, y: u16, text: &str, style: Style) -> u16 {
        self.write_within(x, y, self.remaining(x), text, style)
    }

    /// Writes `text` at `(x, y)` inside a `width`-cell window, returning the
    /// number of cells consumed.
    pub fn write_within(&mut self, x: u16, y: u16, width: u16, text: &str, style: Style) -> u16 {
        if y >= self.area.height || text.is_empty() {
            return 0;
        }
        let budget = width.min(self.remaining(x));
        if budget == 0 {
            return 0;
        }
        let absolute_x = self.area.x.saturating_add(x);
        let absolute_y = self.area.y.saturating_add(y);
        let (end_x, _) =
            self.buffer
                .set_stringn(absolute_x, absolute_y, text, usize::from(budget), style);
        end_x.saturating_sub(absolute_x)
    }

    /// Writes `text` right-aligned inside the `width`-cell window starting at `x`.
    ///
    /// This is how §5.4's right-aligned numeric columns are produced. Text wider
    /// than the window is written from `x` and clipped, so it still cannot escape.
    pub fn write_right(&mut self, x: u16, y: u16, width: u16, text: &str, style: Style) -> u16 {
        let budget = width.min(self.remaining(x));
        let text_width = u16::try_from(display_width(text)).unwrap_or(u16::MAX);
        let pad = budget.saturating_sub(text_width);
        self.write_within(
            x.saturating_add(pad),
            y,
            budget.saturating_sub(pad),
            text,
            style,
        )
    }

    /// Repeats `filler` across row `y` from `x` for `width` cells.
    ///
    /// Used for panel borders and meter tracks. `filler` may be more than one cell
    /// wide; the row is filled to exactly `width` cells where the filler divides
    /// it, and short of it by at most `filler`'s width minus one otherwise.
    pub fn fill_row(&mut self, x: u16, y: u16, width: u16, filler: &str, style: Style) {
        let step = u16::try_from(display_width(filler)).unwrap_or(0);
        if step == 0 {
            return;
        }
        let budget = width.min(self.remaining(x));
        let mut offset = 0u16;
        while offset.saturating_add(step) <= budget {
            let written = self.write_within(x.saturating_add(offset), y, step, filler, style);
            if written == 0 {
                // No room left after clipping; stop rather than spin.
                break;
            }
            offset = offset.saturating_add(written);
        }
    }

    /// Applies `style` to every cell of the painter's area, leaving symbols alone.
    ///
    /// This is the panel-background operation of §5.3's `surface` token. It is a
    /// no-op at zero area.
    pub fn fill_style(&mut self, style: Style) {
        self.buffer.set_style(self.area, style);
    }

    /// Applies `style` to one row of the painter's area.
    ///
    /// Selected and notable process rows use this so the whole row carries the
    /// cue, not only the cells that happened to hold text (§5.2).
    pub fn style_row(&mut self, y: u16, style: Style) {
        if y >= self.area.height {
            return;
        }
        let row = Rect {
            x: self.area.x,
            y: self.area.y.saturating_add(y),
            width: self.area.width,
            height: 1,
        };
        self.buffer.set_style(row, style);
    }

    /// Writes a styled [`Line`] at `(x, y)` inside a `width`-cell window.
    ///
    /// Spans are written left to right and each is clipped in turn, so a line
    /// longer than the window loses its tail rather than overrunning the panel.
    pub fn write_line(&mut self, x: u16, y: u16, width: u16, line: &Line<'_>) -> u16 {
        let mut offset = 0u16;
        for span in &line.spans {
            let budget = width.saturating_sub(offset);
            if budget == 0 {
                break;
            }
            let style = line.style.patch(span.style);
            let written = self.write_within(
                x.saturating_add(offset),
                y,
                budget,
                span.content.as_ref(),
                style,
            );
            if written == 0 && !span.content.is_empty() {
                // The next span cannot fit either: everything left is clipped.
                break;
            }
            offset = offset.saturating_add(written);
        }
        offset
    }

    /// Whether an absolute buffer position is inside this painter's area.
    ///
    /// Exposed for the containment tests; a widget has no reason to ask.
    #[must_use]
    pub const fn covers(&self, position: Position) -> bool {
        self.area.contains(position)
    }
}

/// Assembles one row of styled text left to right within a fixed cell budget.
///
/// Six of the widgets in this module are a single row of labelled fields, and
/// every one of them has to solve the same three problems: reserve a field from
/// the *geometry* rather than from the value in it (§5.4), right-align numerics
/// (§5.4), and stop cleanly when the row runs out of cells (§5.7). Doing that once
/// here is what keeps `meter`, `radar`, `pins`, `cores`, and `table` free of
/// column arithmetic — and it means the width bound is proved once rather than
/// six times.
#[derive(Clone, Debug)]
pub struct RowBuilder {
    width: u16,
    cursor: u16,
    glyphs: GlyphSet,
    spans: Vec<Span<'static>>,
}

impl RowBuilder {
    /// A builder for a row of exactly `width` cells.
    #[must_use]
    pub const fn new(width: u16, glyphs: GlyphSet) -> Self {
        Self {
            width,
            cursor: 0,
            glyphs,
            spans: Vec::new(),
        }
    }

    /// Cells already consumed, which is where the next push lands.
    #[must_use]
    pub const fn cursor(&self) -> u16 {
        self.cursor
    }

    /// Cells still free.
    #[must_use]
    pub const fn remaining(&self) -> u16 {
        self.width.saturating_sub(self.cursor)
    }

    /// Whether the row is already full.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.remaining() == 0
    }

    /// Appends `text`, clipped to whatever room is left. Returns cells consumed.
    ///
    /// Clipping is tail truncation with the glyph mode's own marker, so an
    /// over-long field still reads as "there is more here" (§5.4).
    pub fn push(&mut self, text: &str, style: Style) -> u16 {
        let budget = self.remaining();
        if budget == 0 || text.is_empty() {
            return 0;
        }
        let fitted = fit_within(text, usize::from(budget), self.glyphs);
        let consumed = u16::try_from(display_width(&fitted))
            .unwrap_or(budget)
            .min(budget);
        if consumed == 0 {
            return 0;
        }
        self.spans.push(Span::styled(fitted, style));
        self.cursor = self.cursor.saturating_add(consumed);
        consumed
    }

    /// Appends `text` in a field of exactly `cells`, aligned as `align` asks.
    ///
    /// The field width is the caller's reservation and never the text's own width,
    /// which is what stops a value crossing a unit boundary from moving the fields
    /// to its right (§5.4). A field wider than the row's remainder is shortened to
    /// the remainder rather than dropped, so the row still fills its width.
    pub fn push_field(&mut self, text: &str, cells: u16, align: Align, style: Style) -> u16 {
        let field = cells.min(self.remaining());
        if field == 0 {
            return 0;
        }
        let ellipsis = self.glyphs.ellipsis();
        // Fit first, then pad: `fit_within` blanks a field too narrow to keep any of
        // the text's own characters, so the cell never shows a bare marker fragment
        // that reads as data.
        let fitted = fit_within(text, usize::from(field), self.glyphs);
        let padded = match align {
            Align::Left => pad_right(&fitted, usize::from(field), ellipsis),
            Align::Right => pad_left(&fitted, usize::from(field), ellipsis),
        };
        self.spans.push(Span::styled(padded, style));
        self.cursor = self.cursor.saturating_add(field);
        field
    }

    /// Appends `cells` blank cells. Returns how many were actually available.
    pub fn pad(&mut self, cells: u16) -> u16 {
        let field = cells.min(self.remaining());
        if field == 0 {
            return 0;
        }
        self.spans.push(Span::raw(" ".repeat(usize::from(field))));
        self.cursor = self.cursor.saturating_add(field);
        field
    }

    /// Pads so the next push lands at cell `x`. A cursor already past `x` is left
    /// alone, since text is never rewound.
    pub fn pad_to(&mut self, x: u16) -> u16 {
        self.pad(x.saturating_sub(self.cursor))
    }

    /// Appends `filler` repeatedly for `cells` cells, as a meter track or rule.
    pub fn push_fill(&mut self, filler: &str, cells: u16, style: Style) -> u16 {
        let step = u16::try_from(display_width(filler)).unwrap_or(0);
        if step == 0 {
            return 0;
        }
        let field = cells.min(self.remaining());
        let repeats = field / step;
        if repeats == 0 {
            return 0;
        }
        let text = filler.repeat(usize::from(repeats));
        let consumed = repeats.saturating_mul(step);
        self.spans.push(Span::styled(text, style));
        self.cursor = self.cursor.saturating_add(consumed);
        consumed
    }

    /// The plain text of the row so far, for assertions and snapshots.
    #[must_use]
    pub fn text(&self) -> String {
        self.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// The row padded to its full width, so a caller can assert on a fixed shape.
    #[must_use]
    pub fn padded_text(&self) -> String {
        let mut text = self.text();
        for _ in 0..self.remaining() {
            text.push(' ');
        }
        text
    }

    /// The assembled line.
    #[must_use]
    pub fn finish(self) -> Line<'static> {
        Line::from(self.spans)
    }
}

/// Cells between two segments of a one-line summary, hint strip, or caret note.
pub(crate) const SEGMENT_GAP: usize = 2;

/// Joins `segments` with [`SEGMENT_GAP`] while the total stays inside `budget`.
///
/// Segments are considered in order and a segment that does not fit ends the
/// line, rather than being skipped in favour of a shorter one behind it: the
/// order *is* the priority, and a strip whose fields reordered themselves as
/// values changed would be unreadable (§5.4). A segment is always included
/// whole or not at all — this never clips one down to fit, so the result
/// cannot read as a different, truncated value.
///
/// Lives here rather than in `views` because [`crate::widgets::sparkline::SparklineCaret`]
/// needs it too, and a widget must not depend on the screen layer above it:
/// the caret's two sides only know their own room once the caret's column
/// does, which is geometry `SparklineCaret::row` owns, not `views::caret_note`.
pub(crate) fn join_fitting(segments: &[String], budget: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for segment in segments {
        let width = display_width(segment);
        let gap = if out.is_empty() { 0 } else { SEGMENT_GAP };
        if used + gap + width > budget {
            break;
        }
        for _ in 0..gap {
            out.push(' ');
        }
        out.push_str(segment);
        used += gap + width;
    }
    out
}

#[cfg(test)]
mod row_tests {
    use super::*;
    use ratatui::style::Color;

    fn builder(width: u16) -> RowBuilder {
        RowBuilder::new(width, GlyphSet::ascii())
    }

    #[test]
    fn a_row_never_exceeds_its_width_however_it_is_filled() {
        for width in 0..=40u16 {
            let mut row = builder(width);
            row.push("CPU", Style::new());
            row.push_field("12800%", 6, Align::Right, Style::new());
            row.pad(2);
            row.push_fill("#", 12, Style::new());
            row.push_field(
                "a very long trailing note indeed",
                40,
                Align::Left,
                Style::new(),
            );
            assert!(
                display_width(&row.text()) <= usize::from(width),
                "width {width} produced {:?}",
                row.text()
            );
            assert_eq!(display_width(&row.padded_text()), usize::from(width));
        }
    }

    #[test]
    fn a_reserved_field_keeps_its_width_whatever_the_value_is() {
        // §5.4: `1023B -> 1.0KiB` must not move the columns to its right.
        for value in ["0B", "1023B", "1.0K", "999K", "16E"] {
            let mut row = builder(20);
            row.push_field(value, 6, Align::Right, Style::new());
            row.push_field("|", 1, Align::Left, Style::new());
            assert_eq!(row.cursor(), 7, "{value} shifted the next field");
        }
    }

    #[test]
    fn right_alignment_puts_the_padding_before_the_digits() {
        let mut row = builder(10);
        row.push_field("37%", 6, Align::Right, Style::new());
        assert_eq!(row.text(), "   37%");
        let mut left = builder(10);
        left.push_field("rustc", 8, Align::Left, Style::new());
        assert_eq!(left.text(), "rustc   ");
    }

    #[test]
    fn over_long_text_is_truncated_with_the_glyph_modes_marker() {
        let mut ascii = builder(6);
        ascii.push("permission denied", Style::new());
        assert_eq!(ascii.text(), "per...");
        let mut unicode = RowBuilder::new(6, GlyphSet::unicode());
        unicode.push("permission denied", Style::new());
        assert_eq!(unicode.text(), "permi\u{2026}");
    }

    #[test]
    fn a_full_row_accepts_nothing_more() {
        let mut row = builder(3);
        assert_eq!(row.push("abc", Style::new()), 3);
        assert!(row.is_full());
        assert_eq!(row.push("d", Style::new()), 0);
        assert_eq!(row.push_field("d", 4, Align::Left, Style::new()), 0);
        assert_eq!(row.pad(4), 0);
        assert_eq!(row.push_fill("-", 4, Style::new()), 0);
        assert_eq!(row.text(), "abc");
    }

    #[test]
    fn a_zero_width_row_accepts_nothing_at_all() {
        let mut row = builder(0);
        row.push("anything", Style::new());
        row.push_field("anything", 8, Align::Right, Style::new());
        row.pad(4);
        row.push_fill("#", 8, Style::new());
        assert!(row.text().is_empty());
        assert!(row.padded_text().is_empty());
        assert!(row.finish().spans.is_empty());
    }

    #[test]
    fn a_fill_stops_on_a_glyph_boundary_rather_than_half_a_character() {
        let mut row = builder(20);
        // "+-" is two cells; five cells of room hold two repetitions.
        assert_eq!(row.push_fill("+-", 5, Style::new()), 4);
        assert_eq!(row.text(), "+-+-");
        let mut zero = builder(20);
        assert_eq!(zero.push_fill("", 5, Style::new()), 0);
    }

    #[test]
    fn padding_to_a_position_never_moves_the_cursor_backwards() {
        let mut row = builder(20);
        row.push("abcdef", Style::new());
        assert_eq!(row.pad_to(3), 0);
        assert_eq!(row.cursor(), 6);
        assert_eq!(row.pad_to(9), 3);
        assert_eq!(row.text(), "abcdef   ");
    }

    #[test]
    fn a_double_width_field_still_respects_its_reservation() {
        let mut row = builder(20);
        row.push_field("\u{65e5}\u{672c}\u{8a9e}", 5, Align::Left, Style::new());
        assert_eq!(display_width(&row.text()), 5);
        assert_eq!(row.cursor(), 5);
    }

    #[test]
    fn a_finished_line_carries_the_styles_the_row_was_given() {
        let mut row = builder(10);
        row.push("CPU", Style::new().fg(Color::Red));
        row.push_field("37%", 4, Align::Right, Style::new().fg(Color::Blue));
        let line = row.finish();
        assert_eq!(line.spans.len(), 2);
        assert_eq!(
            line.spans.first().map(|span| span.style.fg),
            Some(Some(Color::Red))
        );
        assert_eq!(
            line.spans.get(1).map(|span| span.style.fg),
            Some(Some(Color::Blue))
        );
    }

    #[test]
    fn a_line_written_through_a_painter_is_clipped_to_the_rectangle() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
        let mut row = builder(20);
        row.push("abcdef", Style::new());
        row.push("ghijkl", Style::new());
        let line = row.finish();
        {
            let mut painter = Painter::new(&mut buffer, Rect::new(2, 0, 6, 1));
            assert_eq!(painter.write_line(0, 0, 20, &line), 6);
        }
        let rendered: String = (0..12)
            .filter_map(|x| buffer.cell((x, 0)).map(|cell| cell.symbol().to_owned()))
            .collect();
        assert_eq!(rendered, "  abcdef    ");
    }

    #[test]
    fn joining_fields_keeps_the_priority_order_and_the_budget() {
        let segments = vec![
            "load 4.12".to_owned(),
            "8 cpu".to_owned(),
            "temp 62C".to_owned(),
        ];
        assert_eq!(join_fitting(&segments, 0), "");
        assert_eq!(join_fitting(&segments, 9), "load 4.12");
        assert_eq!(join_fitting(&segments, 16), "load 4.12  8 cpu");
        assert_eq!(join_fitting(&segments, 100), "load 4.12  8 cpu  temp 62C");
        // A field that does not fit ends the line; a shorter one behind it does
        // not jump the queue, because the order is the priority.
        let wide_then_narrow = vec!["a".repeat(40), "b".to_owned()];
        assert_eq!(join_fitting(&wide_then_narrow, 10), "");
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::*;

    /// A style no widget in this crate uses, so any cell still carrying it was
    /// left untouched.
    fn sentinel() -> Style {
        Style::new()
            .fg(Color::Rgb(1, 2, 3))
            .bg(Color::Rgb(4, 5, 6))
            .add_modifier(Modifier::SLOW_BLINK)
    }

    fn scratch(width: u16, height: u16) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        for cell in &mut buffer.content {
            cell.set_symbol("~");
            cell.set_style(sentinel());
        }
        buffer
    }

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_owned()))
            .collect()
    }

    #[test]
    fn a_write_never_escapes_the_painters_rectangle() {
        let mut buffer = scratch(12, 3);
        {
            let mut painter = Painter::new(&mut buffer, Rect::new(4, 1, 4, 1));
            painter.write(0, 0, "abcdefghij", Style::new());
        }
        assert_eq!(row_text(&buffer, 0), "~~~~~~~~~~~~");
        assert_eq!(row_text(&buffer, 1), "~~~~abcd~~~~");
        assert_eq!(row_text(&buffer, 2), "~~~~~~~~~~~~");
    }

    #[test]
    fn a_row_below_the_rectangle_is_dropped_rather_than_written() {
        let mut buffer = scratch(8, 4);
        {
            let mut painter = Painter::new(&mut buffer, Rect::new(0, 0, 8, 2));
            assert_eq!(painter.write(0, 2, "nope", Style::new()), 0);
            assert_eq!(painter.write(0, 99, "nope", Style::new()), 0);
        }
        for y in 0..4 {
            assert_eq!(row_text(&buffer, y), "~~~~~~~~", "row {y}");
        }
    }

    #[test]
    fn an_area_larger_than_the_buffer_is_clipped_to_the_buffer() {
        let mut buffer = scratch(4, 2);
        let mut painter = Painter::new(&mut buffer, Rect::new(0, 0, 400, 400));
        assert_eq!(painter.area(), Rect::new(0, 0, 4, 2));
        painter.write(0, 0, "abcdef", Style::new());
        assert_eq!(row_text(&buffer, 0), "abcd");
    }

    #[test]
    fn an_off_screen_area_yields_an_empty_painter() {
        let mut buffer = scratch(4, 2);
        let mut painter = Painter::new(&mut buffer, Rect::new(80, 40, 10, 10));
        assert!(painter.is_empty());
        assert_eq!(painter.width(), 0);
        assert_eq!(painter.height(), 0);
        assert_eq!(painter.remaining(0), 0);
        assert_eq!(painter.write(0, 0, "x", Style::new()), 0);
        painter.fill_row(0, 0, 10, "-", Style::new());
        painter.fill_style(Style::new());
        painter.style_row(0, Style::new());
        assert_eq!(row_text(&buffer, 0), "~~~~");
    }

    #[test]
    fn zero_area_rectangles_never_panic() {
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 8, 0),
            Rect::new(0, 0, 0, 8),
            Rect::new(3, 3, 0, 0),
        ] {
            let mut buffer = scratch(8, 8);
            let mut painter = Painter::new(&mut buffer, area);
            assert!(painter.is_empty());
            painter.write(0, 0, "text", Style::new());
            painter.write_right(0, 0, 4, "text", Style::new());
            painter.fill_row(0, 0, 4, "-", Style::new());
            let mut child = painter.sub(Rect::new(0, 0, 4, 4));
            child.write(0, 0, "text", Style::new());
        }
    }

    #[test]
    fn right_alignment_pads_on_the_left_within_the_window() {
        let mut buffer = scratch(10, 1);
        {
            let mut painter = Painter::new(&mut buffer, Rect::new(0, 0, 10, 1));
            painter.write_right(0, 0, 6, "37%", Style::new());
        }
        // The pad is left as it was: right alignment writes text, not spaces, so a
        // caller that wants a cleared column clears it first.
        assert_eq!(row_text(&buffer, 0), "~~~37%~~~~");
    }

    #[test]
    fn right_aligned_text_wider_than_its_window_is_clipped_not_shifted() {
        let mut buffer = scratch(10, 1);
        {
            let mut painter = Painter::new(&mut buffer, Rect::new(0, 0, 10, 1));
            painter.write_right(2, 0, 3, "123456", Style::new());
        }
        assert_eq!(row_text(&buffer, 0), "~~123~~~~~");
    }

    #[test]
    fn a_filler_wider_than_one_cell_never_overshoots_its_window() {
        let mut buffer = scratch(10, 1);
        {
            let mut painter = Painter::new(&mut buffer, Rect::new(0, 0, 10, 1));
            // "+-" is two cells; seven cells of room fit three repetitions.
            painter.fill_row(0, 0, 7, "+-", Style::new());
        }
        assert_eq!(row_text(&buffer, 0), "+-+-+-~~~~");
    }

    #[test]
    fn a_zero_width_filler_is_ignored_instead_of_looping_forever() {
        let mut buffer = scratch(4, 1);
        let mut painter = Painter::new(&mut buffer, Rect::new(0, 0, 4, 1));
        painter.fill_row(0, 0, 4, "", Style::new());
        assert_eq!(row_text(&buffer, 0), "~~~~");
    }

    #[test]
    fn a_double_width_grapheme_that_only_half_fits_is_dropped() {
        let mut buffer = scratch(6, 1);
        {
            let mut painter = Painter::new(&mut buffer, Rect::new(0, 0, 3, 1));
            // Three cells hold one CJK character and then have one cell spare.
            painter.write(0, 0, "日本", Style::new());
        }
        let row = row_text(&buffer, 0);
        assert!(row.starts_with('日'), "{row:?}");
        assert!(
            !row.contains('本'),
            "the second character must not be split"
        );
    }

    #[test]
    fn a_sub_painter_is_confined_to_its_parent() {
        let mut buffer = scratch(10, 3);
        {
            let mut painter = Painter::new(&mut buffer, Rect::new(2, 1, 4, 1));
            let mut child = painter.sub(Rect::new(1, 0, 100, 100));
            assert_eq!(child.area(), Rect::new(3, 1, 3, 1));
            child.write(0, 0, "abcdef", Style::new());
        }
        assert_eq!(row_text(&buffer, 1), "~~~abc~~~~");
    }

    #[test]
    fn styling_a_row_stops_at_the_rectangle_edge() {
        let mut buffer = scratch(6, 2);
        {
            let mut painter = Painter::new(&mut buffer, Rect::new(1, 0, 3, 2));
            painter.style_row(0, Style::new().fg(Color::Red));
            painter.style_row(9, Style::new().fg(Color::Blue));
        }
        let reddened: Vec<u16> = (0..6)
            .filter(|x| {
                buffer
                    .cell((*x, 0))
                    .is_some_and(|cell| cell.fg == Color::Red)
            })
            .collect();
        assert_eq!(reddened, vec![1, 2, 3]);
        assert!(
            (0..6).all(|x| buffer
                .cell((x, 1))
                .is_some_and(|cell| cell.fg != Color::Blue)),
            "a row index past the area must be ignored"
        );
    }

    #[test]
    fn coverage_reports_the_clipped_area_not_the_requested_one() {
        let mut buffer = scratch(8, 4);
        let painter = Painter::new(&mut buffer, Rect::new(2, 1, 40, 40));
        assert!(painter.covers(Position::new(2, 1)));
        assert!(painter.covers(Position::new(7, 3)));
        assert!(!painter.covers(Position::new(1, 1)), "left of the area");
        assert!(!painter.covers(Position::new(2, 0)), "above the area");
        assert!(
            !painter.covers(Position::new(8, 1)),
            "the request was clipped to the buffer, so this is outside"
        );
    }

    #[test]
    fn the_reported_width_is_the_width_actually_consumed() {
        let mut buffer = scratch(10, 1);
        let mut painter = Painter::new(&mut buffer, Rect::new(0, 0, 10, 1));
        assert_eq!(painter.write(0, 0, "abc", Style::new()), 3);
        // Each CJK character occupies two cells.
        assert_eq!(painter.write(0, 0, "日本", Style::new()), 4);
        assert_eq!(painter.write(8, 0, "abcd", Style::new()), 2);
        assert_eq!(painter.write(0, 0, "", Style::new()), 0);
    }
}

//! Placement, framing, and scrolling: the one piece of geometry every overlay
//! shares.
//!
//! Nine overlays would otherwise each solve the same four problems — where to sit,
//! how to stay inside the frame, what to do when there are more lines than rows,
//! and how not to let the screen behind show through. Solving them once here is why
//! the overlays themselves contain no arithmetic: each builds a list of
//! [`Line`]s and hands it to an [`OverlayPanel`].
//!
//! # An overlay is opaque
//!
//! An overlay floats over a drawn screen, so it must *erase* its rectangle rather
//! than merely draw into it: [`OverlayPanel`] fills its whole area with spaces
//! before anything else. Relying on [`crate::theme::Token::Surface`] as a
//! background would work only where colour does — with `--color off` the panel
//! would be transparent and the process table would read straight through the
//! confirmation dialog, which for a §15.1 dialog is not a cosmetic problem.
//!
//! # What never scrolls
//!
//! A panel has three regions, and the scroll offset moves only the middle one:
//!
//! ```text
//! + CONFIRM ------------------------+
//! | NAME          rustc             |  pinned: identity, column headers
//! | PID           31842             |
//! |---------------------------------|
//! | SIGTERM  15  asks the process.. |  body: scrolled by `scroll`
//! | SIGKILL   9  X forceful         |
//! | Y confirm   Esc cancel          |  footer: the §6.2 confirmation key
//! +---------------------------------+
//! ```
//!
//! Rows are reserved footer-first, then pinned, then body. §6.2 requires the
//! confirmation dialog to *show* its explicit confirmation key, so on a terminal
//! too short for everything the key hint is the last thing to go rather than the
//! first.
//!
//! # A clipped line is a lie
//!
//! [`crate::widgets::Painter`] clips a write at the edge of its rectangle, which keeps
//! the containment promise but says nothing to the reader: a sentence that ends because
//! the panel ran out of cells looks exactly like a sentence that ended. §5.4 requires
//! over-long text to be *truncated*, with the glyph mode's own marker, so
//! [`fit_line`] rebuilds any line wider than the body — keeping every span's style, so
//! the §5.2 cues survive — and ends it with the marker.
//!
//! # Scroll clamping matches the reducer
//!
//! [`crate::app::reduce`] clamps a scroll offset to `line_count - 1`, and this
//! module clamps to the same value rather than to `line_count - visible_rows`. The
//! two must agree: clamping harder here would make the last few `j` presses do
//! nothing at all, and clamping less would let the body scroll past its end into
//! blank rows that read as missing data.

use monitrs_core::units::{display_width, truncate_tail};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::glyphs::{Glyph, GlyphSet};
use crate::theme::Token;
use crate::widgets::{Painter, Panel, Presentation};

use super::row::{line_width, widest};

/// Cells the panel border occupies on each side.
const BORDER: u16 = 1;

/// Where an overlay sits inside the area it was handed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Anchor {
    /// Centred both ways: dialogs, reference panels, and anything the user has to
    /// read before acting.
    Center,
    /// Flush with the bottom edge: the one-line editors of §6.2, which sit where
    /// the status line is so that opening them does not move the table above them.
    Bottom,
}

/// The rectangle an overlay occupies, and the rectangle inside its border.
///
/// Both are already clipped to the area the overlay was given, so a caller cannot
/// obtain a body rectangle that is larger than its own frame (§5.7).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OverlayFrame {
    outer: Rect,
    body: Rect,
}

impl OverlayFrame {
    /// Places a `width` × `height` overlay, borders included, inside `area`.
    ///
    /// Both dimensions are clamped to `area`, so an overlay that wants more room
    /// than the terminal has is shrunk rather than pushed off-screen. Every
    /// operation saturates: §5.7 forbids a panic on a calculated rectangle, and a
    /// centring division is exactly where an underflow would hide.
    #[must_use]
    pub fn place(area: Rect, width: u16, height: u16, anchor: Anchor) -> Self {
        // Normalizing through `Rect::new` guarantees `x + width` and `y + height`
        // fit in a `u16`, which makes the arithmetic below safe by construction.
        let area = Rect::new(area.x, area.y, area.width, area.height);
        let width = width.min(area.width);
        let height = height.min(area.height);
        let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
        let y = match anchor {
            Anchor::Center => area
                .y
                .saturating_add(area.height.saturating_sub(height) / 2),
            Anchor::Bottom => area.y.saturating_add(area.height.saturating_sub(height)),
        };
        let outer = Rect {
            x,
            y,
            width,
            height,
        };
        let body = Rect {
            x: outer.x.saturating_add(BORDER),
            y: outer.y.saturating_add(BORDER),
            width: outer.width.saturating_sub(BORDER.saturating_mul(2)),
            height: outer.height.saturating_sub(BORDER.saturating_mul(2)),
        };
        Self { outer, body }
    }

    /// The whole overlay, border included.
    #[must_use]
    pub const fn outer(self) -> Rect {
        self.outer
    }

    /// The rectangle inside the border, where content goes.
    #[must_use]
    pub const fn body(self) -> Rect {
        self.body
    }

    /// Whether there is nowhere to draw at all.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.outer.is_empty()
    }
}

/// How many rows the body's three regions get, for one concrete body height.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Regions {
    pinned: u16,
    separator: u16,
    visible: u16,
    footer: u16,
}

/// A framed, opaque, optionally scrolling overlay.
///
/// The overlay is built from three line lists and rendered into whatever area it
/// is given; it sizes itself from its content and clips itself to the area.
#[derive(Clone, Debug)]
pub struct OverlayPanel<'a> {
    presentation: Presentation<'a>,
    title: &'a str,
    trailing: Option<String>,
    anchor: Anchor,
    pinned: Vec<Line<'static>>,
    lines: Vec<Line<'static>>,
    footer: Vec<Line<'static>>,
    scroll: usize,
    stretched: bool,
}

impl<'a> OverlayPanel<'a> {
    /// An empty panel titled `title`.
    #[must_use]
    pub const fn new(presentation: Presentation<'a>, title: &'a str) -> Self {
        Self {
            presentation,
            title,
            trailing: None,
            anchor: Anchor::Center,
            pinned: Vec::new(),
            lines: Vec::new(),
            footer: Vec::new(),
            scroll: 0,
            stretched: false,
        }
    }

    /// Sets the right-aligned header label, such as `normal mode` or `-00:37`.
    ///
    /// A scrolling panel replaces it with its own `3-14 of 22` indicator: knowing
    /// there is more to read matters more than any label a caller could supply.
    #[must_use]
    pub fn with_trailing(mut self, trailing: impl Into<String>) -> Self {
        self.trailing = Some(trailing.into());
        self
    }

    /// Sets where the panel sits inside its area.
    #[must_use]
    pub const fn anchored(mut self, anchor: Anchor) -> Self {
        self.anchor = anchor;
        self
    }

    /// Sets the lines that stay at the top whatever the scroll offset is.
    #[must_use]
    pub fn with_pinned(mut self, pinned: Vec<Line<'static>>) -> Self {
        self.pinned = pinned;
        self
    }

    /// Sets the scrolling body.
    #[must_use]
    pub fn with_lines(mut self, lines: Vec<Line<'static>>) -> Self {
        self.lines = lines;
        self
    }

    /// Sets the lines that stay at the bottom whatever the scroll offset is.
    #[must_use]
    pub fn with_footer(mut self, footer: Vec<Line<'static>>) -> Self {
        self.footer = footer;
        self
    }

    /// Sets the first visible body line (§6.2's list bindings).
    #[must_use]
    pub const fn with_scroll(mut self, scroll: usize) -> Self {
        self.scroll = scroll;
        self
    }

    /// Makes the panel as wide as its area allows.
    ///
    /// For the one-line editors, whose input box should span the screen the way a
    /// prompt does rather than shrink to the width of what has been typed.
    #[must_use]
    pub const fn stretched(mut self, stretched: bool) -> Self {
        self.stretched = stretched;
        self
    }

    /// How many body lines there are, which is what a scroll offset indexes.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// The width the panel would like, borders included.
    ///
    /// The widest line plus the border, but never so narrow that the panel cannot
    /// name itself: a dialog whose title has been truncated to `+ ... -+` tells the
    /// user nothing about what they are being asked to confirm.
    #[must_use]
    pub fn desired_width(&self) -> u16 {
        let content = widest(&self.pinned)
            .max(widest(&self.lines))
            .max(widest(&self.footer));
        let content = u16::try_from(content).unwrap_or(u16::MAX);
        let header = self.header_width();
        content.saturating_add(BORDER.saturating_mul(2)).max(header)
    }

    /// The width the panel header needs for its title, and its label if it has one.
    ///
    /// Mirrors [`Panel`]'s own header budget: ` TITLE `, ` label `, the three
    /// horizontals before the right corner, one horizontal between them, and two
    /// corners.
    fn header_width(&self) -> u16 {
        let title = u16::try_from(display_width(self.title)).unwrap_or(u16::MAX);
        let bare = title.saturating_add(4);
        match &self.trailing {
            None => bare,
            Some(label) => {
                let label = u16::try_from(display_width(label)).unwrap_or(u16::MAX);
                bare.saturating_add(label).saturating_add(6)
            }
        }
    }

    /// The height the panel would like, borders included.
    #[must_use]
    pub fn desired_height(&self) -> u16 {
        let rows = self
            .pinned
            .len()
            .saturating_add(self.lines.len())
            .saturating_add(self.footer.len())
            .saturating_add(usize::from(self.wants_separator()));
        u16::try_from(rows)
            .unwrap_or(u16::MAX)
            .saturating_add(BORDER.saturating_mul(2))
    }

    /// Whether a rule between the pinned region and the body is warranted.
    const fn wants_separator(&self) -> bool {
        !self.pinned.is_empty() && !self.lines.is_empty()
    }

    /// Where this panel would be drawn inside `area`.
    ///
    /// Exposed so a caller can place a terminal cursor inside the panel — the text
    /// editors need the real cursor, which only the frame owner can set.
    #[must_use]
    pub fn frame(&self, area: Rect) -> OverlayFrame {
        let width = if self.stretched {
            area.width
        } else {
            self.desired_width()
        };
        OverlayFrame::place(area, width, self.desired_height(), self.anchor)
    }

    /// The first body line that is visible at this scroll offset.
    ///
    /// Clamped to the last line rather than to the last *page*, which is the
    /// reducer's rule; see the module documentation.
    #[must_use]
    pub fn scroll_start(&self) -> usize {
        self.scroll.min(self.lines.len().saturating_sub(1))
    }

    /// Splits `height` body rows between the three regions.
    fn regions(&self, height: u16) -> Regions {
        let footer = u16::try_from(self.footer.len())
            .unwrap_or(u16::MAX)
            .min(height);
        let left = height.saturating_sub(footer);
        let pinned = u16::try_from(self.pinned.len())
            .unwrap_or(u16::MAX)
            .min(left);
        let left = left.saturating_sub(pinned);
        // A rule is worth a row only if a body line survives beside it; otherwise
        // it would replace the content it is supposed to separate.
        let separator = u16::from(self.wants_separator() && left > 1);
        Regions {
            pinned,
            separator,
            visible: left.saturating_sub(separator),
            footer,
        }
    }

    /// The header label: the scroll indicator when the body does not fit, and the
    /// caller's own label otherwise.
    fn header_label(&self, visible: u16) -> Option<String> {
        let total = self.lines.len();
        let visible = usize::from(visible);
        if total == 0 || visible == 0 || visible >= total {
            return self.trailing.clone();
        }
        let start = self.scroll_start();
        let end = start.saturating_add(visible).min(total);
        Some(format!("{}-{end} of {total}", start.saturating_add(1)))
    }
}

impl Widget for OverlayPanel<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let frame = self.frame(area);
        if frame.is_empty() {
            return;
        }
        let regions = self.regions(frame.body().height);

        // 1. Erase. An overlay that let the screen behind show through would be a
        //    §15.1 hazard in the confirmation dialog and unreadable everywhere else.
        {
            let mut cover = Painter::new(buf, frame.outer());
            let width = cover.width();
            let style = self.presentation.background(Token::Surface);
            for y in 0..cover.height() {
                cover.fill_row(0, y, width, " ", style);
            }
        }

        // 2. Frame. An open overlay owns the keyboard, so it is the focused panel
        //    (§5.3): the border and title tokens follow from that, not from a
        //    colour choice made here.
        let mut panel = Panel::new(self.presentation, self.title).focused(true);
        let label = self.header_label(regions.visible);
        if let Some(label) = &label {
            panel = panel.with_trailing(label);
        }
        panel.render(frame.outer(), buf);

        // 3. Content.
        let mut painter = Painter::new(buf, frame.body());
        if painter.is_empty() {
            return;
        }
        let width = painter.width();
        let glyphs = self.presentation.glyphs();
        let mut y = 0u16;
        for line in self.pinned.iter().take(usize::from(regions.pinned)) {
            painter.write_line(0, y, width, &fit_line(line, width, glyphs));
            y = y.saturating_add(1);
        }
        if regions.separator > 0 {
            painter.fill_row(
                0,
                y,
                width,
                self.presentation.glyph(Glyph::BorderHorizontal),
                self.presentation.style(Token::Border),
            );
            y = y.saturating_add(1);
        }
        for line in self
            .lines
            .iter()
            .skip(self.scroll_start())
            .take(usize::from(regions.visible))
        {
            painter.write_line(0, y, width, &fit_line(line, width, glyphs));
            y = y.saturating_add(1);
        }
        let footer_top = painter.height().saturating_sub(regions.footer);
        for (offset, line) in self
            .footer
            .iter()
            .take(usize::from(regions.footer))
            .enumerate()
        {
            let Ok(offset) = u16::try_from(offset) else {
                break;
            };
            painter.write_line(
                0,
                footer_top.saturating_add(offset),
                width,
                &fit_line(line, width, glyphs),
            );
        }
    }
}

/// `line` fitted into `width` cells, ending in the glyph mode's truncation marker if
/// it did not fit.
///
/// Spans that fit are kept exactly as they were, so every §5.2 cue and every
/// [`crate::theme::Token`] survives; only the span the line runs out in is rewritten.
/// A line that already fits is returned unchanged, which is the overwhelmingly common
/// case and costs one width measurement.
#[must_use]
pub fn fit_line(line: &Line<'static>, width: u16, glyphs: GlyphSet) -> Line<'static> {
    let budget = usize::from(width);
    if line_width(line) <= budget {
        return line.clone();
    }
    let ellipsis = glyphs.ellipsis();
    // Where the untruncated part has to stop for the marker to fit on the end.
    let content_budget = budget.saturating_sub(ellipsis.width());
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len());
    let mut used = 0usize;
    for span in &line.spans {
        let span_width = display_width(span.content.as_ref());
        if used.saturating_add(span_width) <= content_budget {
            spans.push(span.clone());
            used = used.saturating_add(span_width);
            continue;
        }
        // This is the span the line runs out in: keep what fits and mark the rest.
        let remaining = budget.saturating_sub(used);
        let fitted = truncate_tail(span.content.as_ref(), remaining, ellipsis);
        if !fitted.is_empty() {
            spans.push(Span::styled(fitted, span.style));
        }
        break;
    }
    Line::from(spans)
}

/// The cell a text cursor belongs in, for an editor whose input starts at
/// `(x, y)` inside `body` and whose cursor is `column` cells along.
///
/// Returned as an absolute buffer position so the frame owner can call
/// `Frame::set_cursor_position`; `None` when the cursor would fall outside the
/// body, which is what happens once the typed text is longer than the box.
#[must_use]
pub fn cursor_cell(body: Rect, x: u16, y: u16, column: usize) -> Option<Position> {
    let column = u16::try_from(column).unwrap_or(u16::MAX);
    let offset = x.checked_add(column)?;
    if offset >= body.width || y >= body.height {
        return None;
    }
    Some(Position::new(
        body.x.saturating_add(offset),
        body.y.saturating_add(y),
    ))
}

/// Renders `line_count` and a scroll offset as the `3-14 of 22` header label.
///
/// Exposed for the tests that pin the indicator without rendering a whole panel.
#[must_use]
pub fn scroll_label(scroll: usize, visible: usize, total: usize) -> Option<String> {
    if total == 0 || visible == 0 || visible >= total {
        return None;
    }
    let start = scroll.min(total.saturating_sub(1));
    let end = start.saturating_add(visible).min(total);
    Some(format!("{}-{end} of {total}", start.saturating_add(1)))
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};

    use super::*;
    use crate::theme::{ColorDepth, ThemeId};
    use crate::views::overlays::row::line_width as row_width;

    fn presentation() -> Presentation<'static> {
        Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        )
    }

    fn line(text: &str) -> Line<'static> {
        Line::from(vec![Span::styled(text.to_owned(), Style::new())])
    }

    fn numbered(count: usize) -> Vec<Line<'static>> {
        (0..count)
            .map(|index| line(&format!("line-{index}")))
            .collect()
    }

    fn render(panel: OverlayPanel<'_>, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        panel.render(area, &mut buffer);
        (0..height)
            .map(|y| {
                (0..width)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_owned()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_centred_overlay_sits_in_the_middle_of_its_area() {
        let frame = OverlayFrame::place(Rect::new(0, 0, 40, 20), 10, 4, Anchor::Center);
        assert_eq!(frame.outer(), Rect::new(15, 8, 10, 4));
        assert_eq!(frame.body(), Rect::new(16, 9, 8, 2));
    }

    #[test]
    fn a_bottom_anchored_overlay_sits_on_the_last_row() {
        let frame = OverlayFrame::place(Rect::new(0, 0, 40, 20), 40, 3, Anchor::Bottom);
        assert_eq!(frame.outer(), Rect::new(0, 17, 40, 3));
    }

    #[test]
    fn an_overlay_larger_than_its_area_is_shrunk_rather_than_pushed_off_screen() {
        let area = Rect::new(4, 2, 20, 6);
        let frame = OverlayFrame::place(area, 200, 200, Anchor::Center);
        assert_eq!(frame.outer(), area);
        assert!(area.contains(frame.body().as_position()));
    }

    #[test]
    fn a_zero_area_frame_is_empty_and_never_panics() {
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 20, 0),
            Rect::new(0, 0, 0, 20),
            Rect::new(9, 9, 1, 1),
        ] {
            let frame = OverlayFrame::place(area, 30, 8, Anchor::Center);
            assert!(frame.body().width <= area.width);
            assert!(frame.body().height <= area.height);
        }
        assert!(OverlayFrame::place(Rect::new(0, 0, 0, 0), 30, 8, Anchor::Center).is_empty());
    }

    #[test]
    fn a_panel_sizes_itself_from_its_widest_line() {
        let panel = OverlayPanel::new(presentation(), "T")
            .with_lines(vec![line("short"), line("a rather longer line")]);
        assert_eq!(panel.desired_width(), 22, "20 cells plus two borders");
        assert_eq!(panel.desired_height(), 4);
    }

    #[test]
    fn a_panel_is_never_too_narrow_to_name_itself() {
        let panel = OverlayPanel::new(presentation(), "CONFIRM").with_lines(vec![line("x")]);
        let width = panel.desired_width();
        let rows = render(panel, width, 3);
        assert!(
            rows.first().is_some_and(|row| row.contains("CONFIRM")),
            "{rows:?}"
        );
    }

    #[test]
    fn a_panel_erases_the_screen_behind_it() {
        // §14.3-adjacent, but really a legibility rule: with `--color off` there is no
        // background colour to hide behind, so the panel has to write spaces.
        let area = Rect::new(0, 0, 20, 6);
        let mut buffer = Buffer::empty(area);
        for cell in &mut buffer.content {
            cell.set_symbol("~");
        }
        let panel = OverlayPanel::new(Presentation::default(), "T").with_lines(vec![line("a")]);
        let outer = panel.frame(area).outer();
        panel.render(area, &mut buffer);

        for y in outer.top()..outer.bottom() {
            for x in outer.left()..outer.right() {
                let symbol = buffer
                    .cell((x, y))
                    .map(|cell| cell.symbol().to_owned())
                    .unwrap_or_default();
                assert_ne!(
                    symbol, "~",
                    "the screen behind showed through at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn the_footer_survives_a_terminal_too_short_for_the_body() {
        // §6.2: the dialog must show its confirmation key. Rows are reserved
        // footer-first precisely so that this is the line that stays.
        let panel = OverlayPanel::new(presentation(), "CONFIRM")
            .with_pinned(vec![line("PID 31842")])
            .with_lines(numbered(20))
            .with_footer(vec![line("Y confirm  Esc cancel")]);
        let rows = render(panel, 30, 3);
        assert!(
            rows.get(1).is_some_and(|row| row.contains("Y confirm")),
            "{rows:?}"
        );
    }

    #[test]
    fn pinned_lines_do_not_move_when_the_body_scrolls() {
        let panel = |scroll: usize| {
            OverlayPanel::new(presentation(), "T")
                .with_pinned(vec![line("HEADER")])
                .with_lines(numbered(10))
                .with_scroll(scroll)
        };
        let top = render(panel(0), 24, 6);
        let scrolled = render(panel(4), 24, 6);
        // Row 0 is the frame, row 1 the pinned line, row 2 the rule, rows 3-4 the body.
        assert_eq!(top.get(1), scrolled.get(1), "the pinned row moved");
        assert!(
            top.get(3).is_some_and(|row| row.contains("line-0")),
            "{top:?}"
        );
        assert!(
            scrolled.get(3).is_some_and(|row| row.contains("line-4")),
            "{scrolled:?}"
        );
    }

    #[test]
    fn a_scrolling_panel_says_how_much_more_there_is() {
        let panel = OverlayPanel::new(presentation(), "HELP")
            .with_trailing("normal mode")
            .with_lines(numbered(20));
        let rows = render(panel, 40, 6);
        assert!(rows.iter().any(|row| row.contains("of 20")), "{rows:?}");
    }

    #[test]
    fn a_panel_that_fits_keeps_the_callers_own_label() {
        let panel = OverlayPanel::new(presentation(), "HELP")
            .with_trailing("normal mode")
            .with_lines(numbered(2));
        let rows = render(panel, 40, 6);
        assert!(
            rows.iter().any(|row| row.contains("normal mode")),
            "{rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains(" of ")),
            "a panel that fits must not claim there is more: {rows:?}"
        );
    }

    #[test]
    fn the_scroll_offset_is_clamped_to_the_last_line_as_the_reducer_clamps_it() {
        let panel = OverlayPanel::new(presentation(), "T")
            .with_lines(numbered(10))
            .with_scroll(999);
        assert_eq!(panel.scroll_start(), 9);
        let empty = OverlayPanel::new(presentation(), "T").with_scroll(5);
        assert_eq!(empty.scroll_start(), 0, "no lines means no offset");
    }

    #[test]
    fn a_separator_is_dropped_before_the_last_body_line_is() {
        let panel = OverlayPanel::new(presentation(), "T")
            .with_pinned(vec![line("PINNED")])
            .with_lines(vec![line("BODY")]);
        let regions = panel.regions(2);
        assert_eq!(regions.separator, 0);
        assert_eq!(regions.pinned, 1);
        assert_eq!(regions.visible, 1);
        assert_eq!(panel.regions(3).separator, 1);
    }

    #[test]
    fn a_stretched_panel_takes_the_whole_width() {
        let panel = OverlayPanel::new(presentation(), "FILTER")
            .with_lines(vec![line("/ rustc")])
            .stretched(true);
        assert_eq!(panel.frame(Rect::new(0, 0, 80, 24)).outer().width, 80);
    }

    #[test]
    fn rendering_into_a_zero_area_draws_nothing_and_never_panics() {
        for (width, height) in [(0u16, 0u16), (0, 24), (80, 0), (1, 1), (2, 2), (3, 1)] {
            let panel = OverlayPanel::new(presentation(), "CONFIRM")
                .with_pinned(vec![line("PID")])
                .with_lines(numbered(4))
                .with_footer(vec![line("Esc cancel")]);
            let _ = render(panel, width, height);
        }
    }

    #[test]
    fn a_cursor_outside_the_box_is_reported_as_absent_rather_than_clamped() {
        let body = Rect::new(4, 3, 10, 1);
        assert_eq!(cursor_cell(body, 2, 0, 3), Some(Position::new(9, 3)));
        assert_eq!(cursor_cell(body, 2, 0, 40), None);
        assert_eq!(cursor_cell(body, 2, 5, 0), None);
        assert_eq!(cursor_cell(body, 2, 0, usize::MAX), None);
    }

    #[test]
    fn an_over_long_line_is_truncated_with_a_visible_marker_rather_than_clipped() {
        // §5.4: a sentence that stops because the panel ran out of cells must not read
        // as a sentence that ended.
        let panel = OverlayPanel::new(presentation(), "T").with_lines(vec![line(
            "terminates the process immediately, with no cleanup and no unsaved work",
        )]);
        let rows = render(panel, 24, 3);
        let body = rows.get(1).cloned().unwrap_or_default();
        assert!(body.contains("..."), "{body:?}");
        assert_eq!(display_width(&body), 24);
    }

    #[test]
    fn truncating_a_line_keeps_the_styles_of_the_spans_that_fit() {
        let styled = Line::from(vec![
            Span::styled("KEEP ".to_owned(), Style::new().fg(Color::Red)),
            Span::styled(
                "a very long tail that cannot possibly fit".to_owned(),
                Style::new().fg(Color::Blue),
            ),
        ]);
        let fitted = fit_line(&styled, 12, GlyphSet::ascii());
        assert_eq!(fitted.spans.len(), 2);
        assert_eq!(
            fitted.spans.first().map(|span| span.style.fg),
            Some(Some(Color::Red))
        );
        assert_eq!(
            fitted.spans.get(1).map(|span| span.style.fg),
            Some(Some(Color::Blue)),
            "the truncated span kept its own token"
        );
        assert!(row_width(&fitted) <= 12);
    }

    #[test]
    fn a_line_that_fits_is_returned_untouched() {
        let original = line("short");
        assert_eq!(fit_line(&original, 40, GlyphSet::ascii()), original);
    }

    #[test]
    fn fitting_respects_its_budget_at_every_width_in_both_glyph_modes() {
        let long = Line::from(vec![
            Span::raw("\u{65e5}\u{672c}\u{8a9e}".to_owned()),
            Span::raw("-ascii-tail-that-is-quite-long".to_owned()),
        ]);
        for glyphs in [GlyphSet::ascii(), GlyphSet::unicode()] {
            for width in 0..=48u16 {
                let fitted = fit_line(&long, width, glyphs);
                assert!(
                    row_width(&fitted) <= usize::from(width),
                    "width {width} produced {fitted:?}"
                );
            }
        }
    }

    #[test]
    fn the_scroll_label_appears_only_when_something_is_hidden() {
        assert_eq!(scroll_label(0, 5, 5), None);
        assert_eq!(scroll_label(0, 0, 5), None);
        assert_eq!(scroll_label(0, 5, 0), None);
        assert_eq!(scroll_label(2, 5, 20), Some("3-7 of 20".to_owned()));
        assert_eq!(scroll_label(999, 5, 20), Some("20-20 of 20".to_owned()));
    }
}

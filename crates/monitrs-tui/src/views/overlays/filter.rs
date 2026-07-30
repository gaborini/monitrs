//! The filter editor (§6.2 `/`).
//!
//! One line: the query as it has been typed so far, and how many rows it matches.
//! The match count is the whole point of editing a filter in place rather than
//! submitting it blind — a pattern that matches nothing is a typo, and the user
//! should see that before pressing Enter, not after the table empties.
//!
//! It is anchored to the bottom edge rather than centred, so opening it does not
//! move the table it filters (§6.2 binds `/` to *edit* the filter, and watching the
//! rows change under a stationary cursor is the reason to edit rather than retype).
//!
//! # The cursor is the frame owner's job
//!
//! A widget is handed a [`Buffer`], and only the frame owner can position the
//! terminal's real cursor. [`FilterEditOverlay::cursor_position`] returns where it
//! belongs; a screen that ignores it still gets a legible box, because the cursor
//! cell is also drawn with the selection style (§5.2: the visible cue does not
//! depend on the terminal's own cursor being where we think it is).

use monitrs_core::units::display_width;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::app::{OverlayKind, TextInput};
use crate::theme::Token;
use crate::widgets::{Painter, Presentation};

use super::frame::{Anchor, OverlayPanel, cursor_cell};
use super::row::muted;

/// The prompt that marks the input, matching the `/` that opened it (§6.2).
const PROMPT: &str = "/ ";

/// The filter editor.
#[derive(Clone, Debug)]
pub struct FilterEditOverlay<'a> {
    presentation: Presentation<'a>,
    input: &'a TextInput,
    matches: usize,
    total: usize,
}

impl<'a> FilterEditOverlay<'a> {
    /// An editor over `input`, matching `matches` of `total` processes.
    ///
    /// The counts are the reducer's: `matches` is the number of visible rows and
    /// `total` the process count of the displayed snapshot. This overlay does not
    /// filter anything — [`monitrs_core::process::ProcessFilter`] does, and counting
    /// here would be a second implementation of the same predicate.
    #[must_use]
    pub const fn new(
        presentation: Presentation<'a>,
        input: &'a TextInput,
        matches: usize,
        total: usize,
    ) -> Self {
        Self {
            presentation,
            input,
            matches,
            total,
        }
    }

    /// The match count line, phrased for an empty query as well as a typed one.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.input.is_empty() {
            return format!(
                "{} processes; every row matches an empty filter",
                self.total
            );
        }
        if self.matches == 0 {
            // Distinguishable from "the machine has no processes", which is a real
            // state of its own (§17.3's empty process list).
            return format!("no match of {} processes", self.total);
        }
        format!("{} of {} processes match", self.matches, self.total)
    }

    /// The prompt line: the marker, the query, and the cursor cue.
    #[must_use]
    pub fn input_line(&self) -> Line<'static> {
        let presentation = self.presentation;
        Line::from(vec![
            Span::styled(PROMPT.to_owned(), presentation.style(Token::Accent)),
            Span::styled(
                self.input.text().to_owned(),
                presentation.style(Token::Text),
            ),
        ])
    }

    /// Where the terminal cursor belongs, given the area this overlay was drawn in.
    ///
    /// `None` when the typed text has run past the right edge of the box, which is
    /// the one case where there is no cell to put it in.
    #[must_use]
    pub fn cursor_position(&self, area: Rect) -> Option<Position> {
        let body = self.panel().frame(area).body();
        let column = display_width(PROMPT).saturating_add(self.input.cursor_column());
        cursor_cell(body, 0, 0, column)
    }

    /// The panel this overlay renders through.
    fn panel(&self) -> OverlayPanel<'a> {
        OverlayPanel::new(self.presentation, OverlayKind::FilterEdit.title())
            .anchored(Anchor::Bottom)
            .stretched(true)
            .with_lines(vec![self.input_line()])
            .with_footer(vec![muted(self.presentation, &self.summary())])
    }
}

impl Widget for FilterEditOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let frame = self.panel().frame(area);
        self.panel().render(area, buf);
        // The cursor cue is drawn as well as reported: a screen that never sets the
        // terminal cursor must still show where typing will land (§5.2).
        if let Some(position) = self.cursor_position(area) {
            let mut painter = Painter::new(buf, frame.body());
            let relative = Rect {
                x: position.x.saturating_sub(frame.body().x),
                y: position.y.saturating_sub(frame.body().y),
                width: 1,
                height: 1,
            };
            let selection = self.presentation.selection();
            painter.sub(relative).fill_style(selection.into_style());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::GlyphSet;
    use crate::theme::{ColorDepth, ThemeId};

    fn presentation() -> Presentation<'static> {
        Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        )
    }

    fn render(overlay: FilterEditOverlay<'_>, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        overlay.render(area, &mut buffer);
        (0..height)
            .map(|y| {
                let row: String = (0..width)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_owned()))
                    .collect();
                format!("{row}\n")
            })
            .collect()
    }

    #[test]
    fn the_editor_shows_the_query_and_the_match_count() {
        let input = TextInput::seeded("rustc");
        let overlay = FilterEditOverlay::new(presentation(), &input, 3, 218);
        let text = render(overlay, 60, 6);
        assert!(text.contains("/ rustc"), "{text}");
        assert!(text.contains("3 of 218 processes match"), "{text}");
    }

    #[test]
    fn a_pattern_that_matches_nothing_says_so_distinctly() {
        // "no match" and "no processes" are different facts and must read differently.
        let input = TextInput::seeded("zzz");
        assert_eq!(
            FilterEditOverlay::new(presentation(), &input, 0, 218).summary(),
            "no match of 218 processes"
        );
        let empty = TextInput::new();
        assert_eq!(
            FilterEditOverlay::new(presentation(), &empty, 0, 0).summary(),
            "0 processes; every row matches an empty filter"
        );
    }

    #[test]
    fn the_editor_is_anchored_to_the_bottom_and_spans_the_width() {
        let input = TextInput::seeded("rustc");
        let overlay = FilterEditOverlay::new(presentation(), &input, 1, 4);
        let frame = overlay.panel().frame(Rect::new(0, 0, 80, 24));
        assert_eq!(frame.outer().width, 80);
        assert_eq!(
            frame.outer().y + frame.outer().height,
            24,
            "the editor must sit on the last row"
        );
    }

    #[test]
    fn the_cursor_lands_after_the_text_that_has_been_typed() {
        let input = TextInput::seeded("rustc");
        let overlay = FilterEditOverlay::new(presentation(), &input, 1, 4);
        let area = Rect::new(0, 0, 40, 10);
        let body = overlay.panel().frame(area).body();
        let position = overlay
            .cursor_position(area)
            .expect("the cursor fits in a 40-cell box");
        assert_eq!(
            position.x,
            body.x + u16::try_from(display_width(PROMPT) + 5).expect("small")
        );
        assert_eq!(position.y, body.y);
    }

    #[test]
    fn the_cursor_counts_cells_rather_than_characters() {
        let input = TextInput::seeded("\u{65e5}\u{672c}");
        let overlay = FilterEditOverlay::new(presentation(), &input, 1, 4);
        let area = Rect::new(0, 0, 40, 10);
        let body = overlay.panel().frame(area).body();
        let position = overlay.cursor_position(area).expect("fits");
        assert_eq!(
            position.x,
            body.x + u16::try_from(display_width(PROMPT) + 4).expect("small"),
            "two double-width characters occupy four cells"
        );
    }

    #[test]
    fn a_cursor_past_the_right_edge_is_reported_as_absent() {
        let input = TextInput::seeded(&"x".repeat(200));
        let overlay = FilterEditOverlay::new(presentation(), &input, 1, 4);
        assert_eq!(overlay.cursor_position(Rect::new(0, 0, 40, 10)), None);
    }

    #[test]
    fn the_cursor_cell_is_marked_so_it_shows_without_the_terminal_cursor() {
        let input = TextInput::seeded("ru");
        let area = Rect::new(0, 0, 40, 10);
        let overlay = FilterEditOverlay::new(presentation(), &input, 1, 4);
        let expected = overlay.cursor_position(area).expect("fits");
        let mut buffer = Buffer::empty(area);
        FilterEditOverlay::new(presentation(), &input, 1, 4).render(area, &mut buffer);
        let cell = buffer.cell(expected).expect("inside the buffer");
        let selection = presentation().selection();
        assert_eq!(cell.bg, selection.bg);
    }

    #[test]
    fn the_editor_degrades_and_never_panics() {
        let input = TextInput::seeded("rustc --release");
        for (width, height) in [(80u16, 24u16), (60, 16), (14, 3), (4, 1), (0, 0)] {
            let overlay = FilterEditOverlay::new(presentation(), &input, 2, 9);
            let text = render(overlay, width, height);
            for row in text.lines() {
                assert!(display_width(row) <= usize::from(width), "{row:?}");
            }
        }
    }
}

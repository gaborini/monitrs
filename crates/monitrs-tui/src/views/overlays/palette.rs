//! The command palette overlay (§6.3).
//!
//! The input, and the suggestions that match it. §6.3 exists so that features do not
//! each need a key, which makes the suggestion list the palette's whole reason to be
//! visible: the usage form of each command is how a user learns the grammar without
//! reading the manual.
//!
//! # This overlay does not parse
//!
//! §6.3 requires palette parsing to be deterministic, locally implemented and
//! covered by tests, and [`crate::app::parse_command`] is where that lives. This
//! module calls [`crate::app::hints_for`] to narrow the list and renders what comes
//! back. It never inspects the typed text itself, never decides whether a line is a
//! valid command, and never reports a parse error — an error is a
//! [`crate::app::Notice`] the reducer pushes when the line is *submitted*, and it is
//! rendered by [`super::notice`].

use monitrs_core::units::display_width;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::app::{CommandHint, OverlayKind, TextInput, hints_for};
use crate::glyphs::Glyph;
use crate::theme::Token;
use crate::widgets::{Painter, Presentation};

use super::frame::{Anchor, OverlayPanel, cursor_cell};
use super::row::{muted, styled};

/// The prompt that marks the input, matching the `:` that opened it (§6.2).
const PROMPT: &str = ": ";

/// The command palette.
#[derive(Clone, Debug)]
pub struct CommandPaletteOverlay<'a> {
    presentation: Presentation<'a>,
    input: &'a TextInput,
    highlight: usize,
    hints: Vec<&'static CommandHint>,
}

impl<'a> CommandPaletteOverlay<'a> {
    /// A palette over `input`, with `highlight` selecting one suggestion.
    ///
    /// The suggestions come from [`crate::app::hints_for`], the same function the
    /// reducer completes a half-typed command with, so what is on screen and what
    /// `Enter` would run cannot disagree (§6.3).
    #[must_use]
    pub fn new(presentation: Presentation<'a>, input: &'a TextInput, highlight: usize) -> Self {
        Self {
            presentation,
            input,
            highlight,
            hints: hints_for(input.text()),
        }
    }

    /// The suggestions currently offered.
    #[must_use]
    pub fn hints(&self) -> &[&'static CommandHint] {
        &self.hints
    }

    /// The prompt line: the marker and the line as typed.
    #[must_use]
    pub fn input_line(&self) -> Line<'static> {
        Line::from(vec![
            Span::styled(PROMPT.to_owned(), self.presentation.style(Token::Accent)),
            Span::styled(
                self.input.text().to_owned(),
                self.presentation.style(Token::Text),
            ),
        ])
    }

    /// One row per suggestion: its usage form and what it does.
    #[must_use]
    pub fn suggestion_lines(&self) -> Vec<Line<'static>> {
        if self.hints.is_empty() {
            // Not an error — the reducer decides that on submit — but silence here
            // would read as "the palette is broken" rather than "no such command".
            return vec![muted(
                self.presentation,
                "no command matches what has been typed",
            )];
        }
        let usage_width = self
            .hints
            .iter()
            .map(|hint| display_width(hint.usage))
            .max()
            .unwrap_or(0);
        self.hints
            .iter()
            .enumerate()
            .map(|(index, hint)| {
                let highlighted = index == self.highlight;
                let marker = if highlighted {
                    self.presentation.glyph(Glyph::SelectionMarker)
                } else {
                    self.presentation.glyph(Glyph::SelectionBlank)
                };
                let pad = " ".repeat(
                    usage_width
                        .saturating_sub(display_width(hint.usage))
                        .saturating_add(2),
                );
                styled(
                    self.presentation,
                    &format!("{marker} {}{pad}{}", hint.usage, hint.summary),
                    if highlighted {
                        Token::Accent
                    } else {
                        Token::Text
                    },
                )
            })
            .collect()
    }

    /// Where the terminal cursor belongs, given the area this overlay was drawn in.
    #[must_use]
    pub fn cursor_position(&self, area: Rect) -> Option<Position> {
        let body = self.panel().frame(area).body();
        let column = display_width(PROMPT).saturating_add(self.input.cursor_column());
        cursor_cell(body, 0, 0, column)
    }

    /// The panel this overlay renders through.
    fn panel(&self) -> OverlayPanel<'a> {
        // Stretched rather than content-sized: a prompt whose box narrowed with every
        // keystroke as the suggestion list shrank would be unusable to type in.
        OverlayPanel::new(self.presentation, OverlayKind::CommandPalette.title())
            .with_trailing(format!("{} commands", self.hints.len()))
            .anchored(Anchor::Bottom)
            .with_pinned(vec![self.input_line()])
            .with_lines(self.suggestion_lines())
            .stretched(true)
    }
}

impl Widget for CommandPaletteOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let frame = self.panel().frame(area);
        let cursor = self.cursor_position(area);
        self.panel().render(area, buf);
        if let Some(position) = cursor {
            let mut painter = Painter::new(buf, frame.body());
            let relative = Rect {
                x: position.x.saturating_sub(frame.body().x),
                y: position.y.saturating_sub(frame.body().y),
                width: 1,
                height: 1,
            };
            painter
                .sub(relative)
                .fill_style(self.presentation.selection().into_style());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::COMMAND_HINTS;
    use crate::glyphs::GlyphSet;
    use crate::theme::{ColorDepth, ThemeId};

    fn presentation() -> Presentation<'static> {
        Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        )
    }

    fn render(overlay: CommandPaletteOverlay<'_>, width: u16, height: u16) -> String {
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
    fn an_empty_palette_offers_every_command() {
        let input = TextInput::new();
        let overlay = CommandPaletteOverlay::new(presentation(), &input, 0);
        assert_eq!(overlay.hints().len(), COMMAND_HINTS.len());
        let text = render(overlay, 100, 20);
        for hint in COMMAND_HINTS {
            assert!(
                text.contains(hint.usage),
                "{} is missing:\n{text}",
                hint.usage
            );
        }
    }

    #[test]
    fn the_suggestions_narrow_as_the_command_is_typed() {
        let input = TextInput::seeded("sor");
        let overlay = CommandPaletteOverlay::new(presentation(), &input, 0);
        assert_eq!(overlay.hints().len(), 1);
        let text = render(overlay, 100, 12);
        assert!(text.contains(": sor"), "{text}");
        assert!(text.contains("sort <cpu"), "{text}");
        assert!(!text.contains("reload config"), "{text}");
    }

    #[test]
    fn a_line_that_matches_nothing_says_so_rather_than_going_blank() {
        let input = TextInput::seeded("zzz");
        let overlay = CommandPaletteOverlay::new(presentation(), &input, 0);
        assert!(overlay.hints().is_empty());
        let text = render(overlay, 80, 8);
        assert!(text.contains("no command matches"), "{text}");
    }

    #[test]
    fn the_highlighted_suggestion_is_marked_without_relying_on_colour() {
        // The marker is asserted at the *start* of a row: several usage strings end in
        // `>`, so a substring search would match every line with an argument.
        let input = TextInput::new();
        let overlay = CommandPaletteOverlay::new(Presentation::default(), &input, 2);
        let rows: Vec<String> = overlay
            .suggestion_lines()
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        let marked: Vec<&String> = rows.iter().filter(|row| row.starts_with("> ")).collect();
        assert_eq!(marked.len(), 1, "{rows:?}");
        assert!(
            marked.first().is_some_and(|row| row.contains("filter")),
            "{marked:?}"
        );
    }

    #[test]
    fn the_summaries_are_aligned_in_one_column() {
        // §5.4: a reference list is only readable if its second column lines up.
        let input = TextInput::new();
        let overlay = CommandPaletteOverlay::new(presentation(), &input, 0);
        let columns: Vec<usize> = overlay
            .suggestion_lines()
            .iter()
            .zip(COMMAND_HINTS)
            .filter_map(|(line, hint)| {
                let text: String = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                text.find(hint.summary)
            })
            .collect();
        assert_eq!(columns.len(), COMMAND_HINTS.len());
        let first = columns.first().copied().unwrap_or(0);
        assert!(
            columns.iter().all(|column| *column == first),
            "summaries begin in different columns: {columns:?}"
        );
    }

    #[test]
    fn the_cursor_lands_after_what_has_been_typed() {
        let input = TextInput::seeded("view");
        let overlay = CommandPaletteOverlay::new(presentation(), &input, 0);
        let area = Rect::new(0, 0, 100, 20);
        let body = overlay.panel().frame(area).body();
        let position = overlay.cursor_position(area).expect("fits");
        assert_eq!(
            position.x,
            body.x + u16::try_from(display_width(PROMPT) + 4).expect("small")
        );
        assert_eq!(position.y, body.y);
    }

    #[test]
    fn the_palette_is_anchored_to_the_bottom() {
        let input = TextInput::new();
        let overlay = CommandPaletteOverlay::new(presentation(), &input, 0);
        let frame = overlay.panel().frame(Rect::new(0, 0, 100, 30));
        assert_eq!(frame.outer().y + frame.outer().height, 30);
    }

    #[test]
    fn the_palette_degrades_and_never_panics() {
        let input = TextInput::seeded("export snapshot /tmp/monitrs.json");
        for (width, height) in [(80u16, 24u16), (60, 16), (20, 4), (1, 1), (0, 0)] {
            let overlay = CommandPaletteOverlay::new(presentation(), &input, 0);
            let text = render(overlay, width, height);
            for row in text.lines() {
                assert!(display_width(row) <= usize::from(width), "{row:?}");
            }
        }
    }
}

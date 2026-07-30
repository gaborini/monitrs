//! The sort selector (§6.2 `s`).
//!
//! The column list, with the active column and the direction marked. The list is
//! [`ProcessSortKey::ALL`], which §7.2 already orders by column priority, so the
//! selector shows the most useful sorts nearest the top without deciding anything
//! itself.
//!
//! # Two marks, not one
//!
//! The *highlighted* row and the *active* row are different things and the selector
//! has to distinguish them: the highlight is where `Enter` would land, and the active
//! column is what the table is sorted by right now. Conflating them would make it
//! impossible to see what the current sort is while choosing a new one. The highlight
//! is the selection marker of §5.1; the active column carries the word `active` and
//! its direction, so both survive with colour off (§5.2).
//!
//! # The direction is a word
//!
//! §5.1's glyph inventory has no arrow, and inventing one would break the promise
//! that strict mode emits only what the design system chose. `descending` and
//! `ascending` come from [`monitrs_core::process::SortDirection::label`], read
//! identically in both glyph
//! modes, and say more than an arrow would to someone meeting the selector for the
//! first time.

use monitrs_core::process::{ProcessSort, ProcessSortKey};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::action::ConfirmationKind;
use crate::app::OverlayKind;
use crate::glyphs::Glyph;
use crate::theme::Token;
use crate::widgets::Presentation;

use super::frame::{Anchor, OverlayPanel};
use super::row::{muted, styled};

/// The key that closes an overlay without changing anything (§6.2, every mode).
const CANCEL_HINT: &str = "Esc";

/// The key that reverses the current sort (§6.2 `S`).
const REVERSE_HINT: &str = "S";

/// The sort-column selector.
#[derive(Clone, Debug)]
pub struct SortSelectorOverlay<'a> {
    presentation: Presentation<'a>,
    active: ProcessSort,
    cursor: usize,
}

impl<'a> SortSelectorOverlay<'a> {
    /// A selector showing `active` as the current ordering.
    ///
    /// `cursor` indexes [`ProcessSortKey::ALL`], as
    /// [`crate::app::Overlay::SortSelector`] holds it. An out-of-range cursor simply
    /// highlights nothing rather than being clamped here: the reducer owns the
    /// cursor, and a renderer that silently corrected it would hide the bug.
    #[must_use]
    pub const fn new(presentation: Presentation<'a>, active: ProcessSort, cursor: usize) -> Self {
        Self {
            presentation,
            active,
            cursor,
        }
    }

    /// One row per sortable column.
    #[must_use]
    pub fn lines(&self) -> Vec<Line<'static>> {
        let presentation = self.presentation;
        ProcessSortKey::ALL
            .into_iter()
            .enumerate()
            .map(|(index, key)| {
                let highlighted = index == self.cursor;
                let active = key == self.active.key;
                let marker = if highlighted {
                    presentation.glyph(Glyph::SelectionMarker)
                } else {
                    presentation.glyph(Glyph::SelectionBlank)
                };
                let suffix = if active {
                    format!("  active, {}", self.active.direction.label())
                } else {
                    String::new()
                };
                let token = if active {
                    Token::Accent
                } else if highlighted {
                    Token::Text
                } else {
                    Token::Muted
                };
                styled(
                    presentation,
                    &format!("{marker} {:<16}{suffix}", key.label()),
                    token,
                )
            })
            .collect()
    }

    /// The footer: how to choose, how to reverse, how to leave.
    #[must_use]
    pub fn footer_lines(&self) -> Vec<Line<'static>> {
        vec![muted(
            self.presentation,
            &format!(
                "{} sort by the highlighted column   {REVERSE_HINT} reverse   {CANCEL_HINT} cancel",
                ConfirmationKind::Ordinary.key_hint()
            ),
        )]
    }

    /// The panel this overlay renders through.
    fn panel(&self) -> OverlayPanel<'a> {
        OverlayPanel::new(self.presentation, OverlayKind::SortSelector.title())
            .with_trailing(format!(
                "{}, {}",
                self.active.key.label(),
                self.active.direction.label()
            ))
            .anchored(Anchor::Center)
            .with_lines(self.lines())
            .with_footer(self.footer_lines())
    }

    /// The width the selector would like, borders included.
    #[must_use]
    pub fn desired_width(&self) -> u16 {
        self.panel().desired_width()
    }

    /// The height the selector would like, borders included.
    #[must_use]
    pub fn desired_height(&self) -> u16 {
        self.panel().desired_height()
    }
}

impl Widget for SortSelectorOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.panel().render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use monitrs_core::process::SortDirection;
    use monitrs_core::units::display_width;

    use super::*;
    use crate::glyphs::GlyphSet;
    use crate::keymap::{InputMode, Keymap};
    use crate::theme::{ColorDepth, ThemeId};

    fn presentation() -> Presentation<'static> {
        Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        )
    }

    fn render(overlay: SortSelectorOverlay<'_>, width: u16, height: u16) -> String {
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

    fn full(overlay: SortSelectorOverlay<'_>) -> String {
        let width = overlay.desired_width();
        let height = overlay.desired_height();
        render(overlay, width, height)
    }

    #[test]
    fn every_sortable_column_is_listed() {
        let overlay = SortSelectorOverlay::new(
            presentation(),
            ProcessSort::descending(ProcessSortKey::Cpu),
            0,
        );
        assert_eq!(overlay.lines().len(), ProcessSortKey::ALL.len());
        let text = full(overlay);
        for key in ProcessSortKey::ALL {
            assert!(text.contains(key.label()), "{} is missing:\n{text}", key);
        }
    }

    #[test]
    fn the_active_column_and_its_direction_are_marked() {
        let overlay = SortSelectorOverlay::new(
            presentation(),
            ProcessSort::new(ProcessSortKey::Memory, SortDirection::Ascending),
            0,
        );
        let text = full(overlay);
        // Skipped: the panel header carries the same label as its trailing summary, so
        // the *body* row is the one that has to be marked.
        let row = text
            .lines()
            .skip(1)
            .find(|line| line.contains("memory (RSS)"))
            .expect("the memory row");
        assert!(row.contains("active"), "{row}");
        assert!(row.contains("ascending"), "{row}");
        // Exactly one row is marked active.
        assert_eq!(
            text.lines().filter(|line| line.contains("active")).count(),
            1,
            "{text}"
        );
    }

    #[test]
    fn the_highlight_and_the_active_column_are_distinguishable() {
        // The highlight says where Enter would land; `active` says what the table is
        // sorted by now. Conflating them would hide the current sort.
        let overlay = SortSelectorOverlay::new(
            presentation(),
            ProcessSort::descending(ProcessSortKey::Cpu),
            3,
        );
        let text = full(overlay);
        let highlighted: Vec<&str> = text.lines().filter(|line| line.contains("> ")).collect();
        assert_eq!(highlighted.len(), 1, "{text}");
        assert!(
            highlighted
                .first()
                .is_some_and(|line| line.contains(ProcessSortKey::Pid.label())),
            "{highlighted:?}"
        );
        assert!(
            highlighted
                .first()
                .is_some_and(|line| !line.contains("active")),
            "the highlight was confused with the active column: {highlighted:?}"
        );
    }

    #[test]
    fn the_marks_survive_without_colour() {
        let plain = Presentation::default();
        let overlay =
            SortSelectorOverlay::new(plain, ProcessSort::descending(ProcessSortKey::Cpu), 2);
        let text = full(overlay);
        assert!(text.contains("> "), "the highlight needs a character cue");
        assert!(text.contains("active, descending"), "{text}");
    }

    #[test]
    fn an_out_of_range_cursor_highlights_nothing_rather_than_being_corrected() {
        let overlay = SortSelectorOverlay::new(
            presentation(),
            ProcessSort::descending(ProcessSortKey::Cpu),
            99,
        );
        let text = full(overlay);
        assert!(!text.contains("> "), "{text}");
    }

    #[test]
    fn the_keys_the_footer_names_are_really_bound() {
        let keymap = Keymap::builtin();
        let bound = |label: &str| {
            keymap
                .bindings_for_mode(InputMode::Normal)
                .any(|binding| binding.chord.label() == label)
        };
        assert!(bound(CANCEL_HINT));
        assert!(bound(REVERSE_HINT));
        assert!(bound(ConfirmationKind::Ordinary.key_hint()));
    }

    #[test]
    fn the_selector_degrades_and_never_panics() {
        for (width, height) in [(80u16, 24u16), (60, 16), (12, 4), (1, 1), (0, 0)] {
            let overlay = SortSelectorOverlay::new(
                presentation(),
                ProcessSort::descending(ProcessSortKey::Cpu),
                0,
            );
            let text = render(overlay, width, height);
            for row in text.lines() {
                assert!(display_width(row) <= usize::from(width), "{row:?}");
            }
        }
    }
}

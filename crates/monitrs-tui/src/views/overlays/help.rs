//! The generated help overlay (§7.6).
//!
//! §7.6 is explicit: help is **generated from the active keymap** rather than
//! maintained beside it, and it is context-aware and scrollable. This overlay
//! therefore holds no list of its own. It is handed the
//! [`HelpSection`]s that [`crate::keymap::Keymap::help`] produced for the current
//! [`InputMode`] and renders exactly those, in exactly that order.
//!
//! There is no hand-written footer either, for the same reason: `j`, `k` and `Esc`
//! are keymap rows and appear in the generated body. A footer repeating them would
//! be the second list §7.6 forbids, and it would be the one that goes stale when a
//! binding is reconfigured (§12 allows `[keys]` overrides).
//!
//! # The line count is the reducer's line count
//!
//! [`crate::app::help_line_count`] is what clamps `j` at the bottom of the overlay:
//! one row per section heading plus one per entry. [`HelpOverlay::lines`] produces
//! exactly that shape, so the last line is reachable and no further.

use monitrs_core::units::display_width;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::app::OverlayKind;
use crate::keymap::{HelpSection, InputMode};
use crate::theme::Token;
use crate::widgets::Presentation;

use super::frame::{Anchor, OverlayPanel};
use super::row::{heading, line_width};

/// Cells between the key column and the description.
const KEY_GAP: usize = 2;

/// Cells the entries are indented under their section heading.
const ENTRY_INDENT: usize = 1;

/// The widest the key column is allowed to grow.
///
/// A merged entry such as `Ctrl-D / PageDown` is long, and one very long chord
/// should not push every description off an 80-column terminal. Beyond this the key
/// column keeps its width and the description starts where it always does; the key
/// text itself is never truncated, because a key you cannot read is not a binding
/// you can press.
const MAX_KEY_WIDTH: usize = 20;

/// The context-aware, scrollable help panel.
#[derive(Clone, Debug)]
pub struct HelpOverlay<'a> {
    presentation: Presentation<'a>,
    sections: &'a [HelpSection],
    mode: InputMode,
    scroll: usize,
    version: Option<&'a str>,
}

impl<'a> HelpOverlay<'a> {
    /// A help overlay over the sections generated for `mode`.
    ///
    /// `sections` must be the output of [`crate::keymap::Keymap::help`] for `mode`;
    /// this overlay deliberately cannot generate them itself, which is what makes
    /// §7.6's "never a second list" true by construction.
    #[must_use]
    pub const fn new(
        presentation: Presentation<'a>,
        sections: &'a [HelpSection],
        mode: InputMode,
    ) -> Self {
        Self {
            presentation,
            sections,
            mode,
            scroll: 0,
            version: None,
        }
    }

    /// Shows `version` in the panel's trailing text, beside the input mode.
    ///
    /// Optional, and the caller supplies the string rather than this crate reading
    /// its own `CARGO_PKG_VERSION`: `monitrs-tui` is a library, and a help panel that
    /// reported the *library's* version while the user read it as the application's
    /// would be wrong for anyone who depends on this crate at a different version.
    /// For monitrs itself the two are the same, because every crate in the workspace
    /// shares one version — but that is a fact about that workspace, not about this
    /// widget.
    #[must_use]
    pub const fn with_version(mut self, version: &'a str) -> Self {
        self.version = Some(version);
        self
    }

    /// Sets the first visible line (§6.2's list bindings scroll the overlay).
    #[must_use]
    pub const fn with_scroll(mut self, scroll: usize) -> Self {
        self.scroll = scroll;
        self
    }

    /// How many logical lines the help occupies.
    ///
    /// Equal to [`crate::app::help_line_count`] for the same sections, which is the
    /// bound the reducer clamps scrolling against.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.sections
            .iter()
            .map(|section| section.entries.len().saturating_add(1))
            .sum()
    }

    /// The rendered lines: one heading per section, then one row per entry.
    #[must_use]
    pub fn lines(&self) -> Vec<Line<'static>> {
        let key_width = self.key_width();
        let mut lines = Vec::with_capacity(self.line_count());
        for section in self.sections {
            lines.push(heading(self.presentation, section.title));
            for entry in &section.entries {
                lines.push(self.entry_line(&entry.keys, entry.description, key_width));
            }
        }
        lines
    }

    /// The width of the key column: the widest key text, capped.
    fn key_width(&self) -> usize {
        self.sections
            .iter()
            .flat_map(|section| section.entries.iter())
            .map(|entry| display_width(&entry.keys))
            .max()
            .unwrap_or(0)
            .min(MAX_KEY_WIDTH)
    }

    /// One `  j / Down   Next row` row.
    ///
    /// The keys are the accent and the description is ordinary text, which is the
    /// one accent per row §5.2 allows. A key wider than the column pushes its own
    /// description right rather than being truncated: §5.1's truncation rule is
    /// about *data*, and a binding you cannot type is not help.
    fn entry_line(&self, keys: &str, description: &str, key_width: usize) -> Line<'static> {
        let pad = key_width
            .saturating_sub(display_width(keys))
            .saturating_add(KEY_GAP);
        Line::from(vec![
            Span::raw(" ".repeat(ENTRY_INDENT)),
            Span::styled(keys.to_owned(), self.presentation.style(Token::Accent)),
            Span::raw(" ".repeat(pad)),
            Span::styled(description.to_owned(), self.presentation.style(Token::Text)),
        ])
    }

    /// The panel this overlay renders through.
    fn panel(&self) -> OverlayPanel<'a> {
        let lines = self.lines();
        let body = if lines.is_empty() {
            // A mode with no live bindings cannot happen with the built-in keymap —
            // `Ctrl-C` and `Esc` are bound in every mode — but a configured `[keys]`
            // table could produce it, and an empty panel would look like a bug.
            vec![super::row::muted(
                self.presentation,
                "no keys are bound in this mode",
            )]
        } else {
            lines
        };
        // The version goes in the title, not the trailing label. The trailing label
        // is replaced by the scroll indicator whenever the body does not fit, and a
        // help panel on an 80x24 terminal effectively always scrolls — so a version
        // put there would be hidden in exactly the case someone opened help to find
        // out what they are running.
        let mut panel = OverlayPanel::new(self.presentation, OverlayKind::Help.title());
        if let Some(version) = self.version {
            panel = panel.with_title_suffix(format!("v{version}"));
        }
        panel
            .with_trailing(format!("{} mode", self.mode.label()))
            .anchored(Anchor::Center)
            .with_lines(body)
            .with_scroll(self.scroll)
    }

    /// The width the overlay would like, borders included.
    #[must_use]
    pub fn desired_width(&self) -> u16 {
        self.panel().desired_width()
    }

    /// The widest rendered line, in cells. Exposed for the layout tests.
    #[must_use]
    pub fn content_width(&self) -> usize {
        self.lines().iter().map(line_width).max().unwrap_or(0)
    }
}

impl Widget for HelpOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.panel().render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::help_line_count;
    use crate::glyphs::GlyphSet;
    use crate::keymap::Keymap;
    use crate::theme::{ColorDepth, ThemeId};

    fn presentation() -> Presentation<'static> {
        Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        )
    }

    /// The reason this exists: on 2026-08-02 two monitrs binaries were on one PATH,
    /// one shadowing the other, and nothing on screen said which was running. The
    /// version was reachable only from `--version` or the JSON export's `tool.version`
    /// — neither of which you can consult without leaving the program.
    #[test]
    fn the_version_is_in_the_title_where_scrolling_cannot_hide_it() {
        let keymap = Keymap::default();
        let sections = keymap.help(InputMode::Normal);
        let overlay =
            HelpOverlay::new(presentation(), &sections, InputMode::Normal).with_version("9.9.9");
        let drawn = render(overlay, 80, 24);
        assert!(
            drawn.contains("HELP v9.9.9"),
            "the version belongs on screen, not only behind --version:\n{drawn}"
        );
        // 80x24 cannot show all 46 help lines, so the header shows the scroll range
        // and the trailing label is gone. That is the whole reason the version is in
        // the title: this is the ordinary case, not an edge one.
        assert!(
            drawn.contains("of 46") && !drawn.contains("normal mode"),
            "this fixture is only meaningful while the panel really is scrolling:\n{drawn}"
        );
    }

    #[test]
    fn the_trailing_label_still_shows_the_mode_when_nothing_scrolls() {
        let keymap = Keymap::default();
        let sections = keymap.help(InputMode::ConfirmProcessAction);
        let drawn = render(
            HelpOverlay::new(presentation(), &sections, InputMode::ConfirmProcessAction)
                .with_version("9.9.9"),
            80,
            24,
        );
        assert!(
            drawn.contains("HELP v9.9.9"),
            "the title carries the version either way:\n{drawn}"
        );
        assert!(
            drawn.contains("confirm mode"),
            "and a body that fits keeps the label it always had:\n{drawn}"
        );
    }

    /// The method is an addition, so every existing caller must render exactly as it
    /// did. Without this, "shows the version" could be satisfied by always showing one.
    #[test]
    fn without_a_version_the_panel_is_unchanged() {
        let keymap = Keymap::default();
        let sections = keymap.help(InputMode::Normal);
        let before = render(
            HelpOverlay::new(presentation(), &sections, InputMode::Normal),
            80,
            24,
        );
        // Anchored to the title rather than searching the whole frame: the help body
        // is full of descriptions like "Go to the Overview view", so a bare " v" hits
        // one of those and the assertion passes for the wrong reason.
        assert!(
            before.contains("HELP") && !before.contains("HELP v"),
            "the title gains a suffix only from with_version:\n{before}"
        );
    }

    fn render(overlay: HelpOverlay<'_>, width: u16, height: u16) -> String {
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
    fn every_binding_in_the_keymap_reaches_the_rendered_help() {
        // §7.6's whole point: help is generated, so nothing can be missing from it.
        // Checked against the *rendered buffer* rather than the section list, because
        // a line that exists but is never drawn is not help.
        let keymap = Keymap::builtin();
        for mode in InputMode::ALL {
            let sections = keymap.help(mode);
            let overlay = HelpOverlay::new(presentation(), &sections, mode);
            // Big enough for every line and every column: this test is about
            // coverage, and the degradation is pinned by its own test below.
            let height = u16::try_from(overlay.line_count()).expect("a small keymap") + 2;
            let text = render(overlay, 160, height);

            for binding in keymap.bindings_for_mode(mode) {
                let label = binding.chord.label();
                assert!(
                    text.contains(&label),
                    "`{label}` is bound in {} mode but never reaches the help",
                    mode.label()
                );
                assert!(
                    text.contains(binding.description),
                    "the description of `{label}` in {} mode is missing",
                    mode.label()
                );
            }
        }
    }

    #[test]
    fn the_rendered_line_count_is_the_count_the_reducer_clamps_against() {
        let keymap = Keymap::builtin();
        for mode in InputMode::ALL {
            let sections = keymap.help(mode);
            let overlay = HelpOverlay::new(presentation(), &sections, mode);
            assert_eq!(
                overlay.line_count(),
                help_line_count(&sections),
                "{} mode",
                mode.label()
            );
            assert_eq!(overlay.lines().len(), overlay.line_count());
        }
    }

    #[test]
    fn help_is_context_aware() {
        let keymap = Keymap::builtin();
        let normal = keymap.help(InputMode::Normal);
        let editing = keymap.help(InputMode::FilterEdit);
        let normal_text = render(
            HelpOverlay::new(presentation(), &normal, InputMode::Normal),
            120,
            60,
        );
        let editing_text = render(
            HelpOverlay::new(presentation(), &editing, InputMode::FilterEdit),
            120,
            60,
        );

        assert!(normal_text.contains("Quit"), "{normal_text}");
        assert!(
            !editing_text.contains("Pin or unpin"),
            "a table binding leaked into text-entry help:\n{editing_text}"
        );
        assert!(
            editing_text.contains("Type into the input"),
            "{editing_text}"
        );
        assert!(editing_text.contains("filter mode"), "{editing_text}");
    }

    #[test]
    fn a_long_help_scrolls_and_says_how_far_through_it_is() {
        let keymap = Keymap::builtin();
        let sections = keymap.help(InputMode::Normal);
        let overlay = HelpOverlay::new(presentation(), &sections, InputMode::Normal).with_scroll(0);
        let top = render(overlay, 100, 12);
        assert!(top.contains(" of "), "no scroll indicator:\n{top}");

        let sections = keymap.help(InputMode::Normal);
        let scrolled = HelpOverlay::new(presentation(), &sections, InputMode::Normal)
            .with_scroll(help_line_count(&sections).saturating_sub(1));
        let bottom = render(scrolled, 100, 12);
        assert_ne!(top, bottom, "scrolling changed nothing");
    }

    #[test]
    fn the_last_line_is_reachable_at_the_reducers_maximum_offset() {
        let keymap = Keymap::builtin();
        let sections = keymap.help(InputMode::Normal);
        let last = sections
            .last()
            .and_then(|section| section.entries.last())
            .map(|entry| entry.description)
            .expect("the keymap has entries");
        let overlay = HelpOverlay::new(presentation(), &sections, InputMode::Normal)
            .with_scroll(help_line_count(&sections).saturating_sub(1));
        let text = render(overlay, 100, 12);
        assert!(text.contains(last), "{text}");
    }

    #[test]
    fn the_key_column_is_aligned_across_every_section() {
        let keymap = Keymap::builtin();
        let sections = keymap.help(InputMode::Normal);
        let overlay = HelpOverlay::new(presentation(), &sections, InputMode::Normal);
        let widths: Vec<usize> = overlay
            .lines()
            .iter()
            .filter(|line| line.spans.len() == 4)
            .map(|line| {
                line.spans
                    .iter()
                    .take(3)
                    .map(|span| display_width(span.content.as_ref()))
                    .sum()
            })
            .collect();
        let first = widths.first().copied().unwrap_or(0);
        assert!(first > 0);
        assert!(
            widths.iter().all(|width| *width == first),
            "descriptions start in different columns: {widths:?}"
        );
    }

    #[test]
    fn an_empty_keymap_still_says_something() {
        let overlay = HelpOverlay::new(presentation(), &[], InputMode::Normal);
        assert_eq!(overlay.line_count(), 0);
        let text = render(overlay, 60, 6);
        assert!(text.contains("no keys are bound"), "{text}");
    }

    #[test]
    fn help_degrades_at_eighty_by_twenty_four_without_panicking() {
        let keymap = Keymap::builtin();
        let sections = keymap.help(InputMode::Normal);
        for (width, height) in [(80u16, 24u16), (60, 16), (20, 4), (1, 1), (0, 0)] {
            let overlay = HelpOverlay::new(presentation(), &sections, InputMode::Normal);
            let text = render(overlay, width, height);
            for row in text.lines() {
                assert!(display_width(row) <= usize::from(width), "{row:?}");
            }
        }
    }
}

//! The bordered panel of §5.5: `+ PROCESSES ----- 218 total ---+`.
//!
//! Every border character comes from the [`GlyphSet`], so a panel is box-drawing
//! in enhanced mode and `+`/`-`/`|` in strict ASCII without the panel knowing
//! which (§5.1). Border colour comes from [`Token::Border`] or
//! [`Token::FocusBorder`] and nothing else, which is what makes focus a
//! one-line change rather than a per-panel decision (§5.3).
//!
//! The trailing label is right-aligned and lower priority than the title: when a
//! panel is too narrow for both, the label is dropped rather than the name of the
//! panel. §5.4's rule against reflow is honoured by reserving the label's own
//! width from the *geometry* — the label sits a fixed three cells from the right
//! edge — so a count changing from `99` to `100` moves only the label, never the
//! title.
//!
//! [`GlyphSet`]: crate::glyphs::GlyphSet

use monitrs_core::units::{display_width, truncate_tail};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Borders, Widget};

use crate::glyphs::Glyph;
use crate::theme::Token;
use crate::widgets::{Painter, Presentation};

/// Horizontal cells kept between the trailing label and the right corner, as the
/// `--- 218 total ---+` form of §5.5 shows.
const TRAILING_TAIL: u16 = 3;

/// A bordered panel with a title and an optional right-aligned trailing label.
///
/// The panel draws its frame only. Content goes into [`Panel::inner`], which is
/// what the screen layer passes to the widget that fills it — a panel never
/// borrows the data it surrounds.
#[derive(Clone, Debug)]
pub struct Panel<'a> {
    presentation: Presentation<'a>,
    title: &'a str,
    trailing: Option<&'a str>,
    focused: bool,
    borders: Borders,
    surface: bool,
}

impl<'a> Panel<'a> {
    /// A panel with all four borders and no trailing label.
    #[must_use]
    pub const fn new(presentation: Presentation<'a>, title: &'a str) -> Self {
        Self {
            presentation,
            title,
            trailing: None,
            focused: false,
            borders: Borders::ALL,
            surface: false,
        }
    }

    /// Sets the right-aligned label, such as `218 total` or `5m`.
    #[must_use]
    pub const fn with_trailing(mut self, trailing: &'a str) -> Self {
        self.trailing = Some(trailing);
        self
    }

    /// Marks the panel as holding keyboard focus (§5.3's `focus_border`).
    #[must_use]
    pub const fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// Selects which borders to draw.
    ///
    /// §5.5's dashboard shares borders between vertically adjacent panels — one
    /// row is simultaneously the bottom of the pressure radar and the top of the
    /// process table — so a screen omits the duplicate side rather than drawing
    /// two rows of `+---+`.
    #[must_use]
    pub const fn with_borders(mut self, borders: Borders) -> Self {
        self.borders = borders;
        self
    }

    /// Fills the panel interior with [`Token::Surface`] (§5.3: "panel background
    /// where supported").
    ///
    /// Off by default: on a terminal whose background the user chose deliberately,
    /// repainting it is a downgrade, not a feature.
    #[must_use]
    pub const fn with_surface(mut self, surface: bool) -> Self {
        self.surface = surface;
        self
    }

    /// The rectangle inside the borders.
    ///
    /// Saturating throughout: a panel narrower or shorter than its own frame
    /// returns an empty rectangle rather than underflowing (§5.7).
    #[must_use]
    pub fn inner(&self, area: Rect) -> Rect {
        let left = u16::from(self.borders.contains(Borders::LEFT));
        let right = u16::from(self.borders.contains(Borders::RIGHT));
        let top = u16::from(self.borders.contains(Borders::TOP));
        let bottom = u16::from(self.borders.contains(Borders::BOTTOM));
        Rect {
            x: area.x.saturating_add(left),
            y: area.y.saturating_add(top),
            width: area.width.saturating_sub(left.saturating_add(right)),
            height: area.height.saturating_sub(top.saturating_add(bottom)),
        }
    }

    /// The border token, which is the only place focus becomes a colour.
    #[must_use]
    pub const fn border_token(&self) -> Token {
        Presentation::border_token(self.focused)
    }

    /// The title token: a focused panel's name is the screen's single accent.
    ///
    /// §5.2 caps a numeric row at one accent colour; a panel header is not a
    /// numeric row, but keeping the accent to the focused panel alone means the
    /// eye still has exactly one place to land.
    #[must_use]
    pub const fn title_token(&self) -> Token {
        if self.focused {
            Token::Accent
        } else {
            Token::Text
        }
    }

    /// The header row as it will be rendered at `width` cells.
    ///
    /// Exposed because it is the part worth asserting on directly: the layout
    /// rules — title before label, label three cells from the right, both dropped
    /// before the frame breaks — are easier to pin as a string than as cells.
    #[must_use]
    pub fn header_line(&self, width: u16) -> String {
        let glyphs = self.presentation.glyphs();
        let horizontal = glyphs.get(Glyph::BorderHorizontal);
        let mut cells: Vec<&str> = Vec::new();
        let plan = self.plan(width);

        for index in 0..width {
            cells.push(if plan.left_corner && index == 0 {
                glyphs.get(Glyph::BorderTopLeft)
            } else if plan.right_corner && index.saturating_add(1) == width {
                glyphs.get(Glyph::BorderTopRight)
            } else {
                horizontal
            });
        }
        let mut line: String = cells.concat();

        // The segments are placed by rewriting the string rather than a buffer, so
        // this helper can be asserted on without a terminal.
        if let Some(title) = &plan.title {
            line = overlay(&line, plan.title_x, title);
        }
        if let Some(trailing) = &plan.trailing {
            line = overlay(&line, plan.trailing_x, trailing);
        }
        line
    }

    /// Where the title and trailing segments go on the header row.
    fn plan(&self, width: u16) -> HeaderPlan {
        let has_left = self.borders.contains(Borders::LEFT);
        let has_right = self.borders.contains(Borders::RIGHT);
        let left_corner = has_left && width >= 1;
        // A one-cell panel cannot have two corners; the left one wins so the frame
        // still reads as a left edge.
        let right_corner = has_right && width >= 2;

        let content_x = u16::from(left_corner);
        let content_end = width.saturating_sub(u16::from(right_corner));
        let content_width = content_end.saturating_sub(content_x);
        if content_width == 0 {
            return HeaderPlan {
                left_corner,
                right_corner,
                title: None,
                title_x: content_x,
                trailing: None,
                trailing_x: content_x,
            };
        }

        let ellipsis = self.presentation.ellipsis();
        // ` TITLE `: the surrounding spaces are what separate the name from the
        // horizontal rule, so they are part of the segment rather than padding.
        let title_budget = usize::from(content_width).saturating_sub(2);
        // A truncation that leaves nothing but the marker is worse than no title
        // at all — `+ ... -+` names no panel — so the title needs room for at
        // least one of its own characters beside the marker.
        let title = if self.title.is_empty() || title_budget == 0 {
            None
        } else if display_width(self.title) <= title_budget {
            Some(format!(" {} ", self.title))
        } else if title_budget > ellipsis.width() {
            Some(format!(
                " {} ",
                truncate_tail(self.title, title_budget, ellipsis)
            ))
        } else {
            None
        };
        let title_width = title.as_deref().map_or(0, |text| {
            u16::try_from(display_width(text)).unwrap_or(u16::MAX)
        });

        let mut trailing = None;
        let mut trailing_x = content_end;
        if let Some(label) = self.trailing.filter(|label| !label.is_empty()) {
            let segment = format!(" {label} ");
            let segment_width = u16::try_from(display_width(&segment)).unwrap_or(u16::MAX);
            let needed = segment_width.saturating_add(TRAILING_TAIL);
            // The label is lower priority than the title: it appears only if the
            // title still fits beside it, plus at least one horizontal between them.
            if content_width >= needed.saturating_add(title_width).saturating_add(1) {
                trailing_x = content_end
                    .saturating_sub(TRAILING_TAIL)
                    .saturating_sub(segment_width);
                trailing = Some(segment);
            }
        }

        HeaderPlan {
            left_corner,
            right_corner,
            title,
            title_x: content_x,
            trailing,
            trailing_x,
        }
    }

    fn render_frame(&self, painter: &mut Painter<'_>) {
        let glyphs = self.presentation.glyphs();
        let border_style = self.presentation.style(self.border_token());
        let width = painter.width();
        let height = painter.height();
        let horizontal = glyphs.get(Glyph::BorderHorizontal);
        let vertical = glyphs.get(Glyph::BorderVertical);

        let has_top = self.borders.contains(Borders::TOP);
        // A one-row panel can only carry one horizontal rule, and the header is the
        // one with information in it.
        let has_bottom = self.borders.contains(Borders::BOTTOM) && (!has_top || height >= 2);

        if has_top {
            let plan = self.plan(width);
            painter.fill_row(0, 0, width, horizontal, border_style);
            if plan.left_corner {
                painter.write_within(0, 0, 1, glyphs.get(Glyph::BorderTopLeft), border_style);
            }
            if plan.right_corner {
                painter.write_within(
                    width.saturating_sub(1),
                    0,
                    1,
                    glyphs.get(Glyph::BorderTopRight),
                    border_style,
                );
            }
            if let Some(title) = &plan.title {
                painter.write(
                    plan.title_x,
                    0,
                    title,
                    self.presentation.style(self.title_token()),
                );
            }
            if let Some(trailing) = &plan.trailing {
                painter.write(
                    plan.trailing_x,
                    0,
                    trailing,
                    self.presentation.style(Token::Muted),
                );
            }
        }

        if has_bottom {
            let y = height.saturating_sub(1);
            painter.fill_row(0, y, width, horizontal, border_style);
            if self.borders.contains(Borders::LEFT) {
                painter.write_within(0, y, 1, glyphs.get(Glyph::BorderBottomLeft), border_style);
            }
            if self.borders.contains(Borders::RIGHT) && width >= 2 {
                painter.write_within(
                    width.saturating_sub(1),
                    y,
                    1,
                    glyphs.get(Glyph::BorderBottomRight),
                    border_style,
                );
            }
        }

        let first_side_row = u16::from(has_top);
        let last_side_row = height.saturating_sub(u16::from(has_bottom));
        for y in first_side_row..last_side_row {
            if self.borders.contains(Borders::LEFT) {
                painter.write_within(0, y, 1, vertical, border_style);
            }
            if self.borders.contains(Borders::RIGHT) && width >= 2 {
                painter.write_within(width.saturating_sub(1), y, 1, vertical, border_style);
            }
        }
    }
}

/// The resolved header geometry, so planning and drawing cannot disagree.
struct HeaderPlan {
    left_corner: bool,
    right_corner: bool,
    title: Option<String>,
    title_x: u16,
    trailing: Option<String>,
    trailing_x: u16,
}

/// Replaces `width(segment)` cells of `line` at cell offset `x`.
///
/// Used only by [`Panel::header_line`], which builds a string rather than a
/// buffer. Every character in a border line is one cell wide, and a title wider
/// than the remaining cells has already been truncated, so this cannot split a
/// grapheme.
fn overlay(line: &str, x: u16, segment: &str) -> String {
    let mut out = String::with_capacity(line.len() + segment.len());
    let mut cell = 0usize;
    let start = usize::from(x);
    let end = start + display_width(segment);
    for ch in line.chars() {
        let ch_width = display_width(&ch.to_string()).max(1);
        if cell == start {
            out.push_str(segment);
        }
        if cell < start || cell >= end {
            out.push(ch);
        }
        cell += ch_width;
    }
    if cell <= start {
        out.push_str(segment);
    }
    out
}

impl Widget for Panel<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut painter = Painter::new(buf, area);
        if painter.is_empty() {
            return;
        }
        if self.surface {
            let relative = Rect {
                x: 0,
                y: 0,
                width: painter.width(),
                height: painter.height(),
            };
            let interior = self.inner(relative);
            let mut inner = painter.sub(interior);
            inner.fill_style(self.presentation.background(Token::Surface));
        }
        self.render_frame(&mut painter);
    }
}

#[cfg(test)]
mod tests {
    use crate::glyphs::GlyphSet;
    use crate::theme::{ColorDepth, ThemeId};

    use super::*;

    fn ascii() -> Presentation<'static> {
        Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        )
    }

    fn unicode() -> Presentation<'static> {
        ascii().with_glyphs(GlyphSet::unicode())
    }

    fn render(panel: Panel<'_>, width: u16, height: u16) -> Vec<String> {
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
    fn the_header_matches_the_form_in_the_specification_mockup() {
        // §5.5: `+ PROCESSES ----- 218 total ---+`.
        let panel = Panel::new(ascii(), "PROCESSES").with_trailing("218 total");
        let line = panel.header_line(60);
        assert_eq!(
            line,
            format!("+ PROCESSES {} 218 total ---+", "-".repeat(33))
        );
        assert_eq!(display_width(&line), 60);
    }

    #[test]
    fn the_header_fills_exactly_its_width_at_every_size() {
        for presentation in [ascii(), unicode()] {
            for width in 0..=200u16 {
                let panel = Panel::new(presentation, "PROCESSES").with_trailing("218 total");
                let line = panel.header_line(width);
                assert_eq!(
                    display_width(&line),
                    usize::from(width),
                    "{:?} at width {width} produced {line:?}",
                    presentation.glyphs().style()
                );
            }
        }
    }

    #[test]
    fn the_trailing_label_is_dropped_before_the_title_is() {
        let panel = Panel::new(ascii(), "PROCESSES").with_trailing("218 total");
        // Wide enough for both.
        assert!(panel.header_line(40).contains("218 total"));
        // Narrow: the panel still names itself, the count goes.
        let narrow = panel.header_line(18);
        assert!(narrow.contains("PROCESSES"), "{narrow:?}");
        assert!(!narrow.contains("218"), "{narrow:?}");
        // Narrower still: the title truncates rather than the frame breaking.
        let tiny = panel.header_line(8);
        assert_eq!(display_width(&tiny), 8);
        assert!(tiny.starts_with('+') && tiny.ends_with('+'), "{tiny:?}");
    }

    #[test]
    fn the_trailing_label_sits_a_fixed_distance_from_the_right_edge() {
        // §5.4: reserve from geometry. A count growing a digit must not move the
        // title, only the label.
        let short = Panel::new(ascii(), "PROCESSES").with_trailing("99");
        let long = Panel::new(ascii(), "PROCESSES").with_trailing("1000");
        for line in [short.header_line(50), long.header_line(50)] {
            assert!(line.starts_with("+ PROCESSES "), "{line:?}");
            assert!(line.ends_with("---+"), "{line:?}");
        }
        assert!(short.header_line(50).ends_with(" 99 ---+"));
        assert!(long.header_line(50).ends_with(" 1000 ---+"));
    }

    #[test]
    fn an_empty_trailing_label_is_not_reserved_for() {
        let panel = Panel::new(ascii(), "PINS").with_trailing("");
        let line = panel.header_line(20);
        assert_eq!(line, format!("+ PINS {}+", "-".repeat(12)));
    }

    #[test]
    fn enhanced_mode_draws_box_characters_and_ascii_mode_does_not() {
        let ascii_line = Panel::new(ascii(), "CPU").header_line(12);
        assert_eq!(ascii_line, "+ CPU -----+");
        let unicode_line = Panel::new(unicode(), "CPU").header_line(12);
        assert!(unicode_line.starts_with('\u{250c}'), "{unicode_line:?}");
        assert!(unicode_line.ends_with('\u{2510}'), "{unicode_line:?}");
        assert!(unicode_line.contains('\u{2500}'), "{unicode_line:?}");
        assert!(ascii_line.is_ascii());
    }

    #[test]
    fn a_full_frame_encloses_its_content() {
        let rows = render(Panel::new(ascii(), "P"), 8, 4);
        assert_eq!(
            rows,
            vec![
                "+ P ---+".to_owned(),
                "|      |".to_owned(),
                "|      |".to_owned(),
                "+------+".to_owned(),
            ]
        );
    }

    #[test]
    fn omitting_a_border_leaves_that_side_open_for_a_shared_rule() {
        let rows = render(
            Panel::new(ascii(), "P").with_borders(Borders::ALL.difference(Borders::BOTTOM)),
            8,
            3,
        );
        assert_eq!(
            rows,
            vec![
                "+ P ---+".to_owned(),
                "|      |".to_owned(),
                "|      |".to_owned(),
            ]
        );
    }

    #[test]
    fn the_inner_rectangle_shrinks_by_exactly_the_borders_drawn() {
        let area = Rect::new(3, 5, 20, 10);
        assert_eq!(Panel::new(ascii(), "P").inner(area), Rect::new(4, 6, 18, 8));
        assert_eq!(
            Panel::new(ascii(), "P")
                .with_borders(Borders::TOP)
                .inner(area),
            Rect::new(3, 6, 20, 9)
        );
        assert_eq!(
            Panel::new(ascii(), "P")
                .with_borders(Borders::NONE)
                .inner(area),
            area
        );
    }

    #[test]
    fn an_area_smaller_than_the_frame_yields_an_empty_interior_not_an_underflow() {
        let panel = Panel::new(ascii(), "P");
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, 2, 2),
            Rect::new(9, 9, 1, 2),
        ] {
            let inner = panel.inner(area);
            assert!(inner.width <= area.width && inner.height <= area.height);
        }
        assert!(panel.inner(Rect::new(0, 0, 1, 1)).is_empty());
    }

    #[test]
    fn a_single_row_panel_draws_the_header_rather_than_two_rules() {
        let rows = render(Panel::new(ascii(), "P"), 8, 1);
        assert_eq!(rows, vec!["+ P ---+".to_owned()]);
    }

    #[test]
    fn a_single_column_panel_keeps_one_edge_rather_than_two_corners() {
        let rows = render(Panel::new(ascii(), "P"), 1, 3);
        assert_eq!(rows, vec!["+".to_owned(), "|".to_owned(), "+".to_owned()]);
    }

    #[test]
    fn a_zero_area_panel_draws_nothing_and_does_not_panic() {
        for (width, height) in [(0u16, 0u16), (0, 5), (5, 0)] {
            let rows = render(Panel::new(ascii(), "P").with_trailing("x"), width, height);
            assert!(rows.iter().all(|row| row.trim().is_empty()), "{rows:?}");
        }
    }

    #[test]
    fn focus_changes_the_border_and_title_tokens_and_nothing_else() {
        let unfocused = Panel::new(ascii(), "P");
        let focused = Panel::new(ascii(), "P").focused(true);
        assert_eq!(unfocused.border_token(), Token::Border);
        assert_eq!(focused.border_token(), Token::FocusBorder);
        assert_eq!(unfocused.title_token(), Token::Text);
        assert_eq!(focused.title_token(), Token::Accent);
        // The characters are identical; only the styling differs, so focus cannot
        // shift the layout.
        assert_eq!(unfocused.header_line(30), focused.header_line(30));
    }

    #[test]
    fn the_border_is_styled_with_the_border_token_at_every_depth() {
        for depth in ColorDepth::ALL {
            let presentation = ascii().with_depth(depth);
            let area = Rect::new(0, 0, 10, 3);
            let mut buffer = Buffer::empty(area);
            Panel::new(presentation, "P")
                .focused(true)
                .render(area, &mut buffer);
            let corner = buffer.cell((0u16, 0u16)).expect("the corner");
            let expected = presentation.style(Token::FocusBorder);
            assert_eq!(Some(corner.fg), expected.fg, "{depth:?}");
            assert!(
                corner.modifier.contains(expected.add_modifier),
                "{depth:?} lost the non-colour focus cue"
            );
        }
    }

    #[test]
    fn a_surface_panel_styles_only_its_interior() {
        let area = Rect::new(0, 0, 6, 3);
        let mut buffer = Buffer::empty(area);
        let presentation = ascii();
        Panel::new(presentation, "P")
            .with_surface(true)
            .render(area, &mut buffer);
        let surface = presentation
            .background(Token::Surface)
            .bg
            .expect("a surface colour");
        assert_eq!(buffer.cell((1u16, 1u16)).expect("interior").bg, surface);
        assert_ne!(buffer.cell((0u16, 0u16)).expect("corner").bg, surface);
    }

    #[test]
    fn a_long_title_is_tail_truncated_with_the_glyph_modes_own_marker() {
        let long = Panel::new(ascii(), "VERY LONG PANEL NAME").header_line(16);
        assert_eq!(display_width(&long), 16);
        assert!(long.contains("..."), "{long:?}");
        let unicode_long = Panel::new(unicode(), "VERY LONG PANEL NAME").header_line(16);
        assert!(unicode_long.contains('\u{2026}'), "{unicode_long:?}");
    }

    #[test]
    fn a_double_width_title_never_overflows_the_header() {
        let panel = Panel::new(
            unicode(),
            "\u{65e5}\u{672c}\u{8a9e}\u{30d1}\u{30cd}\u{30eb}",
        );
        for width in 0..=30u16 {
            assert_eq!(display_width(&panel.header_line(width)), usize::from(width));
        }
    }
}

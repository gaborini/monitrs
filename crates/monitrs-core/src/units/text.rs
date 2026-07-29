//! Display-width-aware truncation and padding.
//!
//! Every function here is bounded by a *terminal cell* budget, not a byte or
//! `char` count, because a CJK process name occupies two cells per character
//! and would otherwise overflow its column and corrupt the table.
//!
//! Truncation operates on `char` boundaries rather than grapheme clusters. This
//! is a deliberate scope decision: a combining mark can be separated from its
//! base character in pathological input, but no additional dependency is
//! required and the width budget is still never exceeded.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The marker appended or inserted where text was removed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Ellipsis {
    /// `...` — the only form permitted in strict ASCII mode (§5.1).
    #[default]
    Ascii,
    /// `…` — a single cell, available in enhanced mode.
    Unicode,
}

impl Ellipsis {
    /// The literal marker text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ascii => "...",
            Self::Unicode => "\u{2026}",
        }
    }

    /// The marker's display width in terminal cells.
    #[must_use]
    pub const fn width(self) -> usize {
        match self {
            Self::Ascii => 3,
            Self::Unicode => 1,
        }
    }
}

/// The display width of `text` in terminal cells.
#[must_use]
pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Takes characters from the front of `text` while the total width stays within
/// `budget`, returning the prefix and the width it occupies.
fn take_prefix(text: &str, budget: usize) -> (&str, usize) {
    let mut width = 0usize;
    let mut end = 0usize;
    for (offset, ch) in text.char_indices() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > budget {
            break;
        }
        width += ch_width;
        end = offset + ch.len_utf8();
    }
    (text.get(..end).unwrap_or(""), width)
}

/// Takes characters from the back of `text` while the total width stays within
/// `budget`, returning the suffix and the width it occupies.
fn take_suffix(text: &str, budget: usize) -> (&str, usize) {
    let mut width = 0usize;
    let mut start = text.len();
    for (offset, ch) in text.char_indices().rev() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > budget {
            break;
        }
        width += ch_width;
        start = offset;
    }
    (text.get(start..).unwrap_or(""), width)
}

/// Truncates from the tail, keeping the beginning: `rustc-driver-abc` -> `rustc-d...`.
///
/// Used for executables and process names, where the distinguishing information
/// is at the front (§5.4).
#[must_use]
pub fn truncate_tail(text: &str, max_width: usize, ellipsis: Ellipsis) -> String {
    if display_width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    // Not enough room for content plus a marker: emit a clipped marker so the
    // caller still sees that text was removed.
    if max_width <= ellipsis.width() {
        let (prefix, _) = take_prefix(ellipsis.as_str(), max_width);
        return prefix.to_owned();
    }
    let (prefix, _) = take_prefix(text, max_width - ellipsis.width());
    let mut out = String::with_capacity(prefix.len() + ellipsis.as_str().len());
    out.push_str(prefix);
    out.push_str(ellipsis.as_str());
    out
}

/// Truncates from the middle, keeping both ends:
/// `/Users/lg/pgit/monitrs/target/debug/monitrs` -> `/Users/lg/.../monitrs`.
///
/// Used for full command lines and paths, where the leading directory and the
/// trailing file or argument both carry information (§5.4).
#[must_use]
pub fn truncate_middle(text: &str, max_width: usize, ellipsis: Ellipsis) -> String {
    if display_width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width <= ellipsis.width() {
        let (prefix, _) = take_prefix(ellipsis.as_str(), max_width);
        return prefix.to_owned();
    }
    let content = max_width - ellipsis.width();
    // Bias the extra cell to the head, which usually holds the executable.
    let tail_budget = content / 2;
    let head_budget = content - tail_budget;

    let (head, head_width) = take_prefix(text, head_budget);
    // Reclaim any cell the head could not use (e.g. a double-width character
    // that did not fit) so the result still fills its column.
    let (tail, _) = take_suffix(text, tail_budget + (head_budget - head_width));

    let mut out = String::with_capacity(head.len() + ellipsis.as_str().len() + tail.len());
    out.push_str(head);
    out.push_str(ellipsis.as_str());
    out.push_str(tail);
    // A pathological mix of widths can still overshoot by a cell; clamp.
    if display_width(&out) > max_width {
        return truncate_tail(text, max_width, ellipsis);
    }
    out
}

/// Pads `text` on the left to `width` cells, for right-aligned numeric columns.
///
/// §5.4 requires all numeric columns to be right-aligned. Over-wide input is
/// tail-truncated rather than allowed to break the column.
#[must_use]
pub fn pad_left(text: &str, width: usize, ellipsis: Ellipsis) -> String {
    let text = truncate_tail(text, width, ellipsis);
    let pad = width.saturating_sub(display_width(&text));
    let mut out = String::with_capacity(pad + text.len());
    for _ in 0..pad {
        out.push(' ');
    }
    out.push_str(&text);
    out
}

/// Pads `text` on the right to `width` cells, for left-aligned text columns.
#[must_use]
pub fn pad_right(text: &str, width: usize, ellipsis: Ellipsis) -> String {
    let text = truncate_tail(text, width, ellipsis);
    let pad = width.saturating_sub(display_width(&text));
    let mut out = String::with_capacity(pad + text.len());
    out.push_str(&text);
    for _ in 0..pad {
        out.push(' ');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_returned_unchanged() {
        assert_eq!(truncate_tail("rustc", 10, Ellipsis::Ascii), "rustc");
        assert_eq!(truncate_middle("rustc", 10, Ellipsis::Ascii), "rustc");
    }

    #[test]
    fn tail_truncation_keeps_the_head() {
        assert_eq!(
            truncate_tail("rustc-driver", 9, Ellipsis::Ascii),
            "rustc-..."
        );
        assert_eq!(
            truncate_tail("rustc-driver", 9, Ellipsis::Unicode),
            "rustc-dr\u{2026}"
        );
    }

    #[test]
    fn middle_truncation_keeps_both_ends() {
        let path = "/Users/lg/pgit/monitrs/target/debug/monitrs";
        let out = truncate_middle(path, 20, Ellipsis::Ascii);
        assert_eq!(display_width(&out), 20, "{out:?}");
        assert!(out.starts_with("/Users"), "{out:?}");
        assert!(out.ends_with("monitrs"), "{out:?}");
        assert!(out.contains("..."), "{out:?}");
    }

    #[test]
    fn zero_width_yields_empty_and_never_panics() {
        assert_eq!(truncate_tail("anything", 0, Ellipsis::Ascii), "");
        assert_eq!(truncate_middle("anything", 0, Ellipsis::Ascii), "");
        assert_eq!(pad_left("anything", 0, Ellipsis::Ascii), "");
    }

    #[test]
    fn width_below_the_marker_still_respects_the_budget() {
        for width in 0..=3 {
            let out = truncate_tail("some-long-name", width, Ellipsis::Ascii);
            assert!(display_width(&out) <= width, "width {width} gave {out:?}");
        }
    }

    #[test]
    fn double_width_characters_never_overflow_the_budget() {
        // Each CJK character occupies two cells.
        let name = "日本語のプロセス名";
        for width in 0..=20 {
            let tail = truncate_tail(name, width, Ellipsis::Ascii);
            let middle = truncate_middle(name, width, Ellipsis::Ascii);
            assert!(display_width(&tail) <= width, "tail {width}: {tail:?}");
            assert!(
                display_width(&middle) <= width,
                "middle {width}: {middle:?}"
            );
        }
    }

    #[test]
    fn an_odd_budget_with_double_width_text_is_filled_not_wasted() {
        // Budget 8 = 3 for "..." + 5 content, but CJK cells come in pairs.
        let out = truncate_middle("日本語のプロセス名", 8, Ellipsis::Ascii);
        assert!(display_width(&out) <= 8, "{out:?}");
    }

    #[test]
    fn padding_right_aligns_numeric_columns() {
        assert_eq!(pad_left("287%", 6, Ellipsis::Ascii), "  287%");
        assert_eq!(pad_right("rustc", 8, Ellipsis::Ascii), "rustc   ");
        assert_eq!(display_width(&pad_left("日本", 6, Ellipsis::Ascii)), 6);
    }

    #[test]
    fn padding_truncates_rather_than_breaking_the_column() {
        assert_eq!(
            display_width(&pad_left("1234567890", 5, Ellipsis::Ascii)),
            5
        );
    }
}

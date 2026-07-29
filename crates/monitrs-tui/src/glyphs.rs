//! Glyph modes: strict 7-bit ASCII and enhanced Unicode (§5.1).
//!
//! Every character the interface draws comes from a [`GlyphSet`]. Widgets are
//! forbidden from writing literal box-drawing, block, or Braille characters,
//! because that is the only way to make `--glyphs ascii` a *guarantee* rather
//! than an aspiration: the whole ASCII inventory can then be enumerated and
//! machine-checked (see `every_ascii_glyph_is_printable_seven_bit`).
//!
//! The state characters (`.`, `!`, `X`, `?`) are identical in both modes on
//! purpose. They are the redundant, non-color cue §5.2 requires, and their exact
//! values are fixed by the frozen `MetricState::symbol` and
//! `PressureState::symbol` in `monitrs-core`. Changing them per glyph mode would
//! make the same system state read differently on two terminals.

use core::fmt;
use core::str::FromStr;

use monitrs_core::units::Ellipsis;
use thiserror::Error;

/// The values of `LANG`, `LC_*`, `COLORTERM`, `TERM`, and `NO_COLOR` that
/// capability detection depends on.
///
/// Detection takes this as a parameter instead of reading the process
/// environment so that every branch is reachable from a unit test: mutating
/// `std::env` from tests is racy under the default multi-threaded test harness
/// and would make the suite order-dependent.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TerminalEnv {
    lang: Option<String>,
    lc_all: Option<String>,
    lc_ctype: Option<String>,
    colorterm: Option<String>,
    term: Option<String>,
    no_color: Option<String>,
}

impl TerminalEnv {
    /// An environment in which nothing is set.
    ///
    /// This is the correct starting point for tests: it is also what a process
    /// launched from a minimal `env -i` shell actually sees.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Reads the six variables from the real process environment.
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            lang: std::env::var("LANG").ok(),
            lc_all: std::env::var("LC_ALL").ok(),
            lc_ctype: std::env::var("LC_CTYPE").ok(),
            colorterm: std::env::var("COLORTERM").ok(),
            term: std::env::var("TERM").ok(),
            no_color: std::env::var("NO_COLOR").ok(),
        }
    }

    /// Sets `LANG`.
    #[must_use]
    pub fn with_lang(mut self, value: &str) -> Self {
        self.lang = Some(value.to_owned());
        self
    }

    /// Sets `LC_ALL`.
    #[must_use]
    pub fn with_lc_all(mut self, value: &str) -> Self {
        self.lc_all = Some(value.to_owned());
        self
    }

    /// Sets `LC_CTYPE`.
    #[must_use]
    pub fn with_lc_ctype(mut self, value: &str) -> Self {
        self.lc_ctype = Some(value.to_owned());
        self
    }

    /// Sets `COLORTERM`.
    #[must_use]
    pub fn with_colorterm(mut self, value: &str) -> Self {
        self.colorterm = Some(value.to_owned());
        self
    }

    /// Sets `TERM`.
    #[must_use]
    pub fn with_term(mut self, value: &str) -> Self {
        self.term = Some(value.to_owned());
        self
    }

    /// Sets `NO_COLOR`.
    #[must_use]
    pub fn with_no_color(mut self, value: &str) -> Self {
        self.no_color = Some(value.to_owned());
        self
    }

    /// `COLORTERM`, if set.
    #[must_use]
    pub fn colorterm(&self) -> Option<&str> {
        self.colorterm.as_deref()
    }

    /// `TERM`, if set.
    #[must_use]
    pub fn term(&self) -> Option<&str> {
        self.term.as_deref()
    }

    /// Whether the `NO_COLOR` convention is in force.
    ///
    /// The convention is that the variable counts only when present *and*
    /// non-empty, so `NO_COLOR=` does not disable color.
    #[must_use]
    pub fn no_color_requested(&self) -> bool {
        self.no_color
            .as_deref()
            .is_some_and(|value| !value.is_empty())
    }

    /// The locale category that decides character encoding.
    ///
    /// POSIX precedence: `LC_ALL` overrides `LC_CTYPE`, which overrides `LANG`.
    #[must_use]
    pub fn effective_ctype(&self) -> Option<&str> {
        self.lc_all
            .as_deref()
            .filter(|value| !value.is_empty())
            .or_else(|| self.lc_ctype.as_deref().filter(|value| !value.is_empty()))
            .or_else(|| self.lang.as_deref().filter(|value| !value.is_empty()))
    }

    /// Whether the effective locale declares a UTF-8 codeset.
    ///
    /// Accepts `en_US.UTF-8`, `C.utf8`, and a bare `UTF-8`, which are the forms
    /// glibc, musl, and macOS actually produce.
    #[must_use]
    pub fn is_utf8_locale(&self) -> bool {
        let Some(locale) = self.effective_ctype() else {
            return false;
        };
        // glibc locales may carry a modifier, as in `de_DE.UTF-8@euro`.
        let without_modifier = locale.split('@').next().unwrap_or(locale);
        let codeset = without_modifier
            .rsplit('.')
            .next()
            .unwrap_or(without_modifier);
        let normalized: String = codeset
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        normalized == "utf8"
    }

    /// Whether `TERM` names a terminal that cannot be trusted with anything
    /// beyond plain ASCII.
    ///
    /// An unset `TERM` is treated as unsuitable: §5.1 says `auto` falls back to
    /// ASCII when terminal capabilities are unsuitable, and "no terminal type at
    /// all" is the strongest possible form of that.
    #[must_use]
    pub fn is_dumb_terminal(&self) -> bool {
        match self.term.as_deref() {
            None => true,
            Some(term) => term.is_empty() || term.eq_ignore_ascii_case("dumb"),
        }
    }
}

/// The `--glyphs` setting as requested (§5.1).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum GlyphMode {
    /// Enhanced when the locale and terminal look capable, ASCII otherwise.
    #[default]
    Auto,
    /// Force enhanced mode regardless of detection.
    Unicode,
    /// Force strict 7-bit ASCII regardless of detection.
    Ascii,
}

/// The glyph mode after detection: there is no `Auto` left to interpret.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GlyphStyle {
    /// Box drawing, blocks, Braille, ellipsis, arrows.
    Unicode,
    /// Printable 7-bit ASCII only.
    Ascii,
}

/// The `--glyphs` value was not one of the three documented spellings.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("unknown glyph mode {value:?}; expected one of auto, unicode, ascii")]
pub struct GlyphModeParseError {
    /// The rejected input.
    pub value: String,
}

impl FromStr for GlyphMode {
    type Err = GlyphModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "unicode" | "enhanced" => Ok(Self::Unicode),
            "ascii" => Ok(Self::Ascii),
            _ => Err(GlyphModeParseError {
                value: value.to_owned(),
            }),
        }
    }
}

impl fmt::Display for GlyphMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Unicode => "unicode",
            Self::Ascii => "ascii",
        })
    }
}

impl GlyphMode {
    /// Resolves to a concrete style.
    ///
    /// `Auto` requires *both* a UTF-8 codeset and a `TERM` that is not `dumb`:
    /// a UTF-8 locale under `TERM=dumb` still cannot place box-drawing
    /// characters reliably (§5.1).
    #[must_use]
    pub fn resolve(self, env: &TerminalEnv) -> GlyphStyle {
        match self {
            Self::Unicode => GlyphStyle::Unicode,
            Self::Ascii => GlyphStyle::Ascii,
            Self::Auto => {
                if env.is_utf8_locale() && !env.is_dumb_terminal() {
                    GlyphStyle::Unicode
                } else {
                    GlyphStyle::Ascii
                }
            }
        }
    }

    /// Resolves against the real process environment.
    #[must_use]
    pub fn resolve_from_process(self) -> GlyphStyle {
        self.resolve(&TerminalEnv::from_process())
    }
}

/// One addressable character in the design system.
///
/// The enum exists so that the ASCII inventory is enumerable: [`Glyph::ALL`]
/// plus [`GlyphSet::get`] is the complete set of static characters strict mode
/// can emit, which the purity test iterates exhaustively.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Glyph {
    /// Horizontal panel edge.
    BorderHorizontal,
    /// Vertical panel edge.
    BorderVertical,
    /// Upper-left panel corner.
    BorderTopLeft,
    /// Upper-right panel corner.
    BorderTopRight,
    /// Lower-left panel corner.
    BorderBottomLeft,
    /// Lower-right panel corner.
    BorderBottomRight,
    /// Four-way junction where two panel splits meet.
    BorderCross,
    /// T-junction opening downwards.
    BorderTeeDown,
    /// T-junction opening upwards.
    BorderTeeUp,
    /// T-junction opening leftwards.
    BorderTeeLeft,
    /// T-junction opening rightwards.
    BorderTeeRight,
    /// A completely filled meter cell.
    BarFill,
    /// A partially filled meter cell.
    BarPartial,
    /// An empty meter cell whose value *is* known to be below it.
    BarEmpty,
    /// A meter cell whose value is not known at all (§4: never render 0%).
    BarTrack,
    /// Left meter bracket.
    MeterOpen,
    /// Right meter bracket.
    MeterClose,
    /// Tree connector for a child with following siblings.
    TreeBranch,
    /// Tree connector for the last child.
    TreeLast,
    /// Tree continuation through an ancestor level.
    TreeVertical,
    /// Tree indentation past an exhausted ancestor level.
    TreeIndent,
    /// The marker on the selected row.
    SelectionMarker,
    /// The same width as [`Glyph::SelectionMarker`], for unselected rows.
    SelectionBlank,
    /// `PressureState::Normal`.
    StateNormal,
    /// `PressureState::Watch`.
    StateWatch,
    /// `PressureState::Critical`.
    StateCritical,
    /// A state that could not be determined.
    StateUnknown,
    /// The text shown where a metric has no value.
    Unavailable,
    /// The marker left where text was removed.
    Truncation,
}

impl Glyph {
    /// The number of distinct glyphs.
    pub const COUNT: usize = 29;

    /// Every glyph, in a fixed order that the tests pin down.
    pub const ALL: [Self; Self::COUNT] = [
        Self::BorderHorizontal,
        Self::BorderVertical,
        Self::BorderTopLeft,
        Self::BorderTopRight,
        Self::BorderBottomLeft,
        Self::BorderBottomRight,
        Self::BorderCross,
        Self::BorderTeeDown,
        Self::BorderTeeUp,
        Self::BorderTeeLeft,
        Self::BorderTeeRight,
        Self::BarFill,
        Self::BarPartial,
        Self::BarEmpty,
        Self::BarTrack,
        Self::MeterOpen,
        Self::MeterClose,
        Self::TreeBranch,
        Self::TreeLast,
        Self::TreeVertical,
        Self::TreeIndent,
        Self::SelectionMarker,
        Self::SelectionBlank,
        Self::StateNormal,
        Self::StateWatch,
        Self::StateCritical,
        Self::StateUnknown,
        Self::Unavailable,
        Self::Truncation,
    ];

    /// A stable position, used only to prove `ALL` is complete.
    ///
    /// Test-only, because it exists for that proof and for nothing else. The
    /// match is exhaustive, so adding a variant fails to compile until it is
    /// given an index, and `all_lists_every_glyph_exactly_once` then fails until
    /// it is added to `ALL`.
    #[cfg(test)]
    const fn index(self) -> usize {
        match self {
            Self::BorderHorizontal => 0,
            Self::BorderVertical => 1,
            Self::BorderTopLeft => 2,
            Self::BorderTopRight => 3,
            Self::BorderBottomLeft => 4,
            Self::BorderBottomRight => 5,
            Self::BorderCross => 6,
            Self::BorderTeeDown => 7,
            Self::BorderTeeUp => 8,
            Self::BorderTeeLeft => 9,
            Self::BorderTeeRight => 10,
            Self::BarFill => 11,
            Self::BarPartial => 12,
            Self::BarEmpty => 13,
            Self::BarTrack => 14,
            Self::MeterOpen => 15,
            Self::MeterClose => 16,
            Self::TreeBranch => 17,
            Self::TreeLast => 18,
            Self::TreeVertical => 19,
            Self::TreeIndent => 20,
            Self::SelectionMarker => 21,
            Self::SelectionBlank => 22,
            Self::StateNormal => 23,
            Self::StateWatch => 24,
            Self::StateCritical => 25,
            Self::StateUnknown => 26,
            Self::Unavailable => 27,
            Self::Truncation => 28,
        }
    }
}

/// The nine-level strict-ASCII sparkline ramp named by §5.1.
const ASCII_RAMP: [char; 9] = ['.', ':', '-', '=', '+', '*', '#', '%', '@'];

/// The eight-level block ramp used in enhanced mode.
const BLOCK_RAMP: [char; 8] = [
    '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}',
];

/// Left-aligned eighth blocks, for the fractional cell of a horizontal meter.
const EIGHTHS: [&str; 7] = [
    "\u{258f}", "\u{258e}", "\u{258d}", "\u{258c}", "\u{258b}", "\u{258a}", "\u{2589}",
];

/// Braille dot bits for the left column, bottom row first.
const BRAILLE_LEFT: [u8; 4] = [0x40, 0x04, 0x02, 0x01];

/// Braille dot bits for the right column, bottom row first.
const BRAILLE_RIGHT: [u8; 4] = [0x80, 0x20, 0x10, 0x08];

/// The base of the Braille Patterns block.
const BRAILLE_BASE: u32 = 0x2800;

/// The resolved character set widgets draw with.
///
/// Cheap to copy; holds no state beyond the resolved style, because §5.2 forbids
/// anything that alternates or animates.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GlyphSet {
    style: GlyphStyle,
}

impl GlyphSet {
    /// Builds a set for an already-resolved style.
    #[must_use]
    pub const fn new(style: GlyphStyle) -> Self {
        Self { style }
    }

    /// Builds a set by resolving `mode` against `env`.
    #[must_use]
    pub fn resolve(mode: GlyphMode, env: &TerminalEnv) -> Self {
        Self::new(mode.resolve(env))
    }

    /// The strict-ASCII set.
    #[must_use]
    pub const fn ascii() -> Self {
        Self::new(GlyphStyle::Ascii)
    }

    /// The enhanced Unicode set.
    #[must_use]
    pub const fn unicode() -> Self {
        Self::new(GlyphStyle::Unicode)
    }

    /// The resolved style.
    #[must_use]
    pub const fn style(self) -> GlyphStyle {
        self.style
    }

    /// Whether this set is restricted to printable 7-bit ASCII.
    #[must_use]
    pub const fn is_ascii(self) -> bool {
        matches!(self.style, GlyphStyle::Ascii)
    }

    /// The text for one glyph.
    #[must_use]
    pub const fn get(self, glyph: Glyph) -> &'static str {
        match self.style {
            GlyphStyle::Ascii => Self::ascii_text(glyph),
            GlyphStyle::Unicode => Self::unicode_text(glyph),
        }
    }

    /// The strict-ASCII spelling. Every arm is `0x20..=0x7e` by construction and
    /// by test.
    const fn ascii_text(glyph: Glyph) -> &'static str {
        match glyph {
            Glyph::BorderHorizontal => "-",
            Glyph::BorderVertical => "|",
            Glyph::BorderTopLeft
            | Glyph::BorderTopRight
            | Glyph::BorderBottomLeft
            | Glyph::BorderBottomRight
            | Glyph::BorderCross
            | Glyph::BorderTeeDown
            | Glyph::BorderTeeUp
            | Glyph::BorderTeeLeft
            | Glyph::BorderTeeRight => "+",
            Glyph::BarFill => "#",
            Glyph::BarPartial => "=",
            Glyph::BarEmpty => "-",
            Glyph::BarTrack => ".",
            Glyph::MeterOpen => "[",
            Glyph::MeterClose => "]",
            Glyph::TreeBranch => "+-",
            Glyph::TreeLast => "`-",
            Glyph::TreeVertical => "| ",
            Glyph::TreeIndent => "  ",
            Glyph::SelectionMarker => ">",
            Glyph::SelectionBlank => " ",
            Glyph::StateNormal => ".",
            Glyph::StateWatch => "!",
            Glyph::StateCritical => "X",
            Glyph::StateUnknown => "?",
            Glyph::Unavailable => "n/a",
            Glyph::Truncation => "...",
        }
    }

    /// The enhanced spelling.
    ///
    /// The four state characters and `n/a` are intentionally the same as in
    /// strict mode: they are fixed by the frozen `symbol()` methods in
    /// `monitrs-core` and by `MetricState::placeholder`.
    const fn unicode_text(glyph: Glyph) -> &'static str {
        match glyph {
            Glyph::BorderHorizontal => "\u{2500}",
            Glyph::BorderVertical => "\u{2502}",
            Glyph::BorderTopLeft => "\u{250c}",
            Glyph::BorderTopRight => "\u{2510}",
            Glyph::BorderBottomLeft => "\u{2514}",
            Glyph::BorderBottomRight => "\u{2518}",
            Glyph::BorderCross => "\u{253c}",
            Glyph::BorderTeeDown => "\u{252c}",
            Glyph::BorderTeeUp => "\u{2534}",
            Glyph::BorderTeeLeft => "\u{2524}",
            Glyph::BorderTeeRight => "\u{251c}",
            Glyph::BarFill => "\u{2588}",
            Glyph::BarPartial => "\u{258c}",
            Glyph::BarEmpty => "\u{2591}",
            Glyph::BarTrack => "\u{00b7}",
            Glyph::MeterOpen => "[",
            Glyph::MeterClose => "]",
            Glyph::TreeBranch => "\u{251c}\u{2500}",
            Glyph::TreeLast => "\u{2514}\u{2500}",
            Glyph::TreeVertical => "\u{2502} ",
            Glyph::TreeIndent => "  ",
            Glyph::SelectionMarker => "\u{25b8}",
            Glyph::SelectionBlank => " ",
            Glyph::StateNormal => ".",
            Glyph::StateWatch => "!",
            Glyph::StateCritical => "X",
            Glyph::StateUnknown => "?",
            Glyph::Unavailable => "n/a",
            Glyph::Truncation => "\u{2026}",
        }
    }

    /// The horizontal panel edge.
    #[must_use]
    pub const fn border_horizontal(self) -> &'static str {
        self.get(Glyph::BorderHorizontal)
    }

    /// The vertical panel edge.
    #[must_use]
    pub const fn border_vertical(self) -> &'static str {
        self.get(Glyph::BorderVertical)
    }

    /// The tree connector for a child that has following siblings.
    #[must_use]
    pub const fn tree_branch(self) -> &'static str {
        self.get(Glyph::TreeBranch)
    }

    /// The tree connector for the last child at its level.
    #[must_use]
    pub const fn tree_last(self) -> &'static str {
        self.get(Glyph::TreeLast)
    }

    /// The tree continuation through an ancestor that has more children.
    #[must_use]
    pub const fn tree_vertical(self) -> &'static str {
        self.get(Glyph::TreeVertical)
    }

    /// The tree indentation past an ancestor with no further children.
    #[must_use]
    pub const fn tree_indent(self) -> &'static str {
        self.get(Glyph::TreeIndent)
    }

    /// The selected-row marker.
    #[must_use]
    pub const fn selection_marker(self) -> &'static str {
        self.get(Glyph::SelectionMarker)
    }

    /// The unselected-row filler, the same width as the marker.
    #[must_use]
    pub const fn selection_blank(self) -> &'static str {
        self.get(Glyph::SelectionBlank)
    }

    /// The text shown in place of a metric that has no value (§4).
    #[must_use]
    pub const fn unavailable(self) -> &'static str {
        self.get(Glyph::Unavailable)
    }

    /// The truncation marker, as the width-aware form `monitrs-core` expects.
    #[must_use]
    pub const fn ellipsis(self) -> Ellipsis {
        match self.style {
            GlyphStyle::Ascii => Ellipsis::Ascii,
            GlyphStyle::Unicode => Ellipsis::Unicode,
        }
    }

    /// The sparkline ramp, lowest level first.
    #[must_use]
    pub fn sparkline_ramp(self) -> &'static [char] {
        match self.style {
            GlyphStyle::Ascii => &ASCII_RAMP,
            GlyphStyle::Unicode => &BLOCK_RAMP,
        }
    }

    /// Renders `samples` as a sparkline occupying exactly `width` cells.
    ///
    /// The most recent samples win: a longer history is cropped from the left so
    /// the right-hand edge is always "now", matching the §5.5 history panel.
    /// A history shorter than `width` is padded with spaces, never with the
    /// lowest ramp character — "no sample yet" must not read as "zero" (§4).
    ///
    /// `max` is the value that maps to the top of the ramp. Passing a fixed
    /// ceiling (100.0 for a percentage) keeps the plot comparable between
    /// frames; passing the observed maximum makes it self-scaling.
    #[must_use]
    pub fn sparkline(self, samples: &[f32], width: usize, max: f32) -> String {
        if width == 0 {
            return String::new();
        }
        let ramp = self.sparkline_ramp();
        let visible = tail(samples, width);
        let mut out = String::with_capacity(width * 4);
        for _ in 0..width.saturating_sub(visible.len()) {
            out.push(' ');
        }
        for &sample in visible {
            match fraction_of(sample, max) {
                Some(fraction) => out.push(ramp_char(ramp, fraction)),
                None => out.push(' '),
            }
        }
        out
    }

    /// Renders `samples` at double horizontal resolution where the glyph mode
    /// allows it.
    ///
    /// Enhanced mode packs two samples per cell into a Braille pattern, which is
    /// what §5.1 permits Braille for. Strict ASCII has no denser form than the
    /// nine-level ramp, so it degrades to [`GlyphSet::sparkline`] rather than
    /// inventing characters.
    #[must_use]
    pub fn dense_sparkline(self, samples: &[f32], width: usize, max: f32) -> String {
        if matches!(self.style, GlyphStyle::Ascii) {
            return self.sparkline(samples, width, max);
        }
        if width == 0 {
            return String::new();
        }
        let capacity = width.saturating_mul(2);
        let visible = tail(samples, capacity);
        // Align to cell boundaries from the right so the newest sample lands in
        // the right half of the rightmost cell.
        let cells_used = visible.len().div_ceil(2);
        let mut out = String::with_capacity(width * 3);
        for _ in 0..width.saturating_sub(cells_used) {
            out.push(' ');
        }
        // An odd count leaves the oldest sample alone in its cell's left column.
        let offset = usize::from(!visible.len().is_multiple_of(2));
        for cell in 0..cells_used {
            let mut bits = 0u8;
            for column in 0..2usize {
                let position = cell * 2 + column;
                if position < offset {
                    continue;
                }
                let Some(&sample) = visible.get(position - offset) else {
                    continue;
                };
                let Some(fraction) = fraction_of(sample, max) else {
                    continue;
                };
                let dots = braille_dots(fraction);
                let column_bits = if column == 0 {
                    &BRAILLE_LEFT
                } else {
                    &BRAILLE_RIGHT
                };
                for bit in column_bits.iter().take(dots) {
                    bits |= *bit;
                }
            }
            // `0x2800..=0x28ff` are all assigned, so the fallback is unreachable;
            // it exists only because `from_u32` cannot be told that.
            out.push(char::from_u32(BRAILLE_BASE + u32::from(bits)).unwrap_or('\u{2800}'));
        }
        out
    }

    /// Renders a horizontal bar of exactly `width` cells for `fraction`.
    ///
    /// `fraction` is clamped into `0.0..=1.0`, so a process CPU percentage above
    /// 100% fills the bar rather than overflowing it (§5.4). A non-finite or
    /// negative `fraction` is a calculation error rather than a measurement, and
    /// renders as [`GlyphSet::unknown_bar`] instead of as an honest-looking 0%.
    #[must_use]
    pub fn bar(self, fraction: f32, width: usize) -> String {
        if width == 0 {
            return String::new();
        }
        if !fraction.is_finite() || fraction < 0.0 {
            return self.unknown_bar(width);
        }
        let fraction = fraction.min(1.0);
        let fill = self.get(Glyph::BarFill);
        let empty = self.get(Glyph::BarEmpty);
        let mut out = String::with_capacity(width * 3);
        // Cell boundaries are compared in floating point rather than derived by
        // casting a scaled float to an integer: no cast means no truncation or
        // sign-loss hazard, and `width` here is a terminal dimension.
        let cells = width as f32;
        for cell in 0..width {
            let lower = cell as f32 / cells;
            let upper = (cell + 1) as f32 / cells;
            if fraction >= upper {
                out.push_str(fill);
            } else if fraction > lower {
                out.push_str(self.partial_cell((fraction - lower) * cells));
            } else {
                out.push_str(empty);
            }
        }
        out
    }

    /// The character for a cell that is `within` (`0.0..1.0`) full.
    ///
    /// Strict ASCII has exactly one partial character, so a half-filled cell and
    /// a nearly-full cell look the same; enhanced mode resolves the same cell to
    /// one eighth of a character, which is why an enhanced meter reads smoothly
    /// while an ASCII meter steps.
    fn partial_cell(self, within: f32) -> &'static str {
        match self.style {
            GlyphStyle::Ascii => self.get(Glyph::BarPartial),
            GlyphStyle::Unicode => {
                let steps = EIGHTHS.len();
                let mut chosen = 0usize;
                for step in 1..steps {
                    if within >= step as f32 / steps as f32 {
                        chosen = step;
                    }
                }
                EIGHTHS
                    .get(chosen)
                    .copied()
                    .unwrap_or_else(|| self.get(Glyph::BarPartial))
            }
        }
    }

    /// Renders a bar for a metric that has no value at all.
    ///
    /// This is the rendering for `WarmingUp`, `PermissionDenied`, `Unsupported`,
    /// and `TemporarilyUnavailable`. It is deliberately *not* an empty bar,
    /// because an empty bar means "measured, and it is zero" (§4).
    #[must_use]
    pub fn unknown_bar(self, width: usize) -> String {
        let track = self.get(Glyph::BarTrack);
        let mut out = String::with_capacity(width * 3);
        for _ in 0..width {
            out.push_str(track);
        }
        out
    }

    /// Renders a bracketed meter of exactly `width` cells, as in §5.5.
    ///
    /// Below three cells there is no room for both brackets and any bar, so the
    /// brackets are dropped in favour of showing the value.
    #[must_use]
    pub fn meter(self, fraction: f32, width: usize) -> String {
        match width.checked_sub(2) {
            Some(inner) if width >= 3 => self.bracket(&self.bar(fraction, inner)),
            _ => self.bar(fraction, width),
        }
    }

    /// Renders a bracketed meter for a metric with no value.
    #[must_use]
    pub fn unknown_meter(self, width: usize) -> String {
        match width.checked_sub(2) {
            Some(inner) if width >= 3 => self.bracket(&self.unknown_bar(inner)),
            _ => self.unknown_bar(width),
        }
    }

    fn bracket(self, inner: &str) -> String {
        let open = self.get(Glyph::MeterOpen);
        let close = self.get(Glyph::MeterClose);
        let mut out = String::with_capacity(inner.len() + open.len() + close.len());
        out.push_str(open);
        out.push_str(inner);
        out.push_str(close);
        out
    }
}

/// The last `count` elements of `samples`, or all of them when there are fewer.
fn tail(samples: &[f32], count: usize) -> &[f32] {
    let start = samples.len().saturating_sub(count);
    samples.get(start..).unwrap_or(samples)
}

/// Normalizes `sample` against `max`, or `None` when there is nothing to plot.
///
/// `None` means "no usable value" and is rendered as blank. A `max` of zero is
/// different: a flat-zero history is real data, and renders at the bottom of the
/// ramp rather than disappearing.
fn fraction_of(sample: f32, max: f32) -> Option<f32> {
    if !sample.is_finite() || sample < 0.0 || !max.is_finite() || max < 0.0 {
        return None;
    }
    if max <= 0.0 {
        return Some(0.0);
    }
    Some((sample / max).clamp(0.0, 1.0))
}

/// Picks the ramp character for a `0.0..=1.0` fraction by nearest level.
fn ramp_char(ramp: &[char], fraction: f32) -> char {
    let Some(&lowest) = ramp.first() else {
        return ' ';
    };
    let top = ramp.len().saturating_sub(1);
    if top == 0 {
        return lowest;
    }
    let mut chosen = 0usize;
    for level in 1..=top {
        // Round to the nearest level: the midpoint between level-1 and level.
        if fraction >= (level as f32 - 0.5) / top as f32 {
            chosen = level;
        }
    }
    ramp.get(chosen).copied().unwrap_or(lowest)
}

/// How many of the four Braille dot rows to light for a `0.0..=1.0` fraction.
///
/// Always at least one: a measured zero must still be visible, so only a missing
/// sample leaves a cell column blank.
fn braille_dots(fraction: f32) -> usize {
    let mut dots = 1usize;
    for level in 1..4usize {
        if fraction >= (level as f32 - 0.5) / 3.0 {
            dots = level + 1;
        }
    }
    dots
}

#[cfg(test)]
mod tests {
    use monitrs_core::units::display_width;

    use super::*;

    const BOTH: [GlyphSet; 2] = [GlyphSet::ascii(), GlyphSet::unicode()];

    #[test]
    fn all_lists_every_glyph_exactly_once() {
        assert_eq!(Glyph::ALL.len(), Glyph::COUNT);
        for (position, glyph) in Glyph::ALL.iter().enumerate() {
            assert_eq!(
                glyph.index(),
                position,
                "{glyph:?} is missing from or misplaced in Glyph::ALL"
            );
        }
    }

    #[test]
    fn every_ascii_glyph_is_printable_seven_bit() {
        let ascii = GlyphSet::ascii();
        for glyph in Glyph::ALL {
            let text = ascii.get(glyph);
            assert!(!text.is_empty(), "{glyph:?} has no text");
            for byte in text.bytes() {
                assert!(
                    (0x20..=0x7e).contains(&byte),
                    "{glyph:?} = {text:?} contains byte {byte:#04x}"
                );
            }
        }
        for &level in ascii.sparkline_ramp() {
            assert!(
                level.is_ascii_graphic(),
                "ramp character {level:?} is not printable ASCII"
            );
        }
        for byte in ascii.ellipsis().as_str().bytes() {
            assert!((0x20..=0x7e).contains(&byte));
        }
    }

    #[test]
    fn every_string_strict_ascii_can_render_is_printable_seven_bit() {
        let ascii = GlyphSet::ascii();
        let adversarial = [
            0.0,
            -0.0,
            1.0,
            0.5,
            100.0,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            -3.5,
            f32::MAX,
            f32::MIN_POSITIVE,
        ];
        let mut rendered: Vec<String> = Vec::new();
        for width in 0..=40usize {
            for value in adversarial {
                rendered.push(ascii.bar(value, width));
                rendered.push(ascii.meter(value, width));
                rendered.push(ascii.sparkline(&[value; 7], width, 100.0));
                rendered.push(ascii.sparkline(&adversarial, width, value));
                rendered.push(ascii.dense_sparkline(&adversarial, width, value));
            }
            rendered.push(ascii.unknown_bar(width));
            rendered.push(ascii.unknown_meter(width));
            rendered.push(ascii.sparkline(&[], width, 100.0));
        }
        for text in rendered {
            for byte in text.bytes() {
                assert!(
                    (0x20..=0x7e).contains(&byte),
                    "rendered {text:?} contains byte {byte:#04x}"
                );
            }
        }
    }

    #[test]
    fn enhanced_mode_actually_uses_non_ascii_glyphs() {
        // Guards against the enhanced table silently regressing to ASCII.
        let unicode = GlyphSet::unicode();
        assert!(!unicode.border_horizontal().is_ascii());
        assert!(!unicode.get(Glyph::BarFill).is_ascii());
        assert!(!unicode.get(Glyph::Truncation).is_ascii());
        assert!(unicode.sparkline_ramp().iter().any(|c| !c.is_ascii()));
        assert!(!unicode.dense_sparkline(&[1.0, 2.0, 3.0], 4, 3.0).is_ascii());
    }

    #[test]
    fn state_characters_match_the_frozen_core_symbols_in_both_modes() {
        use monitrs_core::model::{MetricState, PressureState};
        for set in BOTH {
            assert_eq!(
                set.get(Glyph::StateNormal),
                PressureState::Normal.symbol().to_string()
            );
            assert_eq!(
                set.get(Glyph::StateWatch),
                PressureState::Watch.symbol().to_string()
            );
            assert_eq!(
                set.get(Glyph::StateCritical),
                PressureState::Critical.symbol().to_string()
            );
            let unavailable: MetricState<u64> = MetricState::TemporarilyUnavailable(
                monitrs_core::model::UnavailableReason::ReadFailed,
            );
            assert_eq!(
                set.get(Glyph::StateUnknown),
                unavailable.symbol().to_string()
            );
            let unsupported: MetricState<u64> = MetricState::Unsupported;
            assert_eq!(set.unavailable(), unsupported.placeholder().expect("n/a"));
        }
    }

    #[test]
    fn tree_and_selection_glyphs_have_the_same_width_in_both_modes() {
        // Otherwise a tree switching glyph mode would shift every row's indent.
        for glyph in [
            Glyph::TreeBranch,
            Glyph::TreeLast,
            Glyph::TreeVertical,
            Glyph::TreeIndent,
            Glyph::SelectionMarker,
            Glyph::SelectionBlank,
            Glyph::BorderHorizontal,
            Glyph::BorderVertical,
            Glyph::BarFill,
            Glyph::BarEmpty,
            Glyph::BarTrack,
        ] {
            assert_eq!(
                display_width(GlyphSet::ascii().get(glyph)),
                display_width(GlyphSet::unicode().get(glyph)),
                "{glyph:?} changes width between glyph modes"
            );
        }
    }

    #[test]
    fn sparklines_occupy_exactly_the_requested_width() {
        let samples: Vec<f32> = (0..200).map(|i| i as f32).collect();
        for set in BOTH {
            for width in 0..=40usize {
                for slice in [&samples[..], &samples[..3], &[][..]] {
                    let plain = set.sparkline(slice, width, 199.0);
                    assert_eq!(
                        display_width(&plain),
                        width,
                        "{:?} sparkline of {} samples at width {width}: {plain:?}",
                        set.style(),
                        slice.len()
                    );
                    let dense = set.dense_sparkline(slice, width, 199.0);
                    assert_eq!(
                        display_width(&dense),
                        width,
                        "{:?} dense sparkline at width {width}: {dense:?}",
                        set.style()
                    );
                }
            }
        }
    }

    #[test]
    fn a_shorter_history_is_blank_padded_rather_than_shown_as_zero() {
        let ascii = GlyphSet::ascii();
        let rendered = ascii.sparkline(&[100.0, 100.0], 6, 100.0);
        assert_eq!(rendered, "    @@");
        // The lowest ramp character means "measured zero", not "no sample".
        let zeros = ascii.sparkline(&[0.0, 0.0], 6, 100.0);
        assert_eq!(zeros, "    ..");
    }

    #[test]
    fn the_newest_sample_is_always_the_rightmost_cell() {
        let ascii = GlyphSet::ascii();
        let samples = [0.0, 0.0, 0.0, 100.0];
        let rendered = ascii.sparkline(&samples, 2, 100.0);
        assert_eq!(rendered, ".@");
    }

    #[test]
    fn a_flat_zero_history_still_plots_instead_of_vanishing() {
        // `max == 0` is a real, flat-zero series and must not blank the plot.
        let ascii = GlyphSet::ascii();
        assert_eq!(ascii.sparkline(&[0.0, 0.0, 0.0], 3, 0.0), "...");
    }

    #[test]
    fn non_finite_samples_render_as_blank_not_as_the_floor() {
        let ascii = GlyphSet::ascii();
        let rendered = ascii.sparkline(&[f32::NAN, 50.0, -1.0], 3, 100.0);
        assert_eq!(rendered, " + ");
    }

    #[test]
    fn bars_occupy_exactly_the_requested_width() {
        for set in BOTH {
            for width in 0..=40usize {
                for step in 0..=20 {
                    let fraction = step as f32 / 20.0;
                    let bar = set.bar(fraction, width);
                    assert_eq!(
                        display_width(&bar),
                        width,
                        "{:?} bar {fraction} at width {width}: {bar:?}",
                        set.style()
                    );
                    let meter = set.meter(fraction, width);
                    assert_eq!(display_width(&meter), width, "{meter:?}");
                }
                assert_eq!(display_width(&set.unknown_bar(width)), width);
                assert_eq!(display_width(&set.unknown_meter(width)), width);
            }
        }
    }

    #[test]
    fn a_full_bar_is_entirely_filled_and_an_empty_bar_entirely_not() {
        let ascii = GlyphSet::ascii();
        assert_eq!(ascii.bar(1.0, 5), "#####");
        assert_eq!(ascii.bar(0.0, 5), "-----");
        // Values above 100% clamp instead of overflowing the column (§5.4).
        assert_eq!(ascii.bar(2.87, 5), "#####");
        assert_eq!(ascii.bar(0.5, 4), "##--");
    }

    #[test]
    fn an_unknown_bar_is_visually_distinct_from_a_zero_bar() {
        for set in BOTH {
            let zero = set.bar(0.0, 8);
            let unknown = set.unknown_bar(8);
            assert_ne!(
                zero,
                unknown,
                "{:?} conflates unknown with zero",
                set.style()
            );
        }
    }

    #[test]
    fn an_invalid_fraction_renders_as_unknown_rather_than_zero() {
        let ascii = GlyphSet::ascii();
        assert_eq!(ascii.bar(f32::NAN, 4), ascii.unknown_bar(4));
        assert_eq!(ascii.bar(-1.0, 4), ascii.unknown_bar(4));
        assert_eq!(ascii.bar(f32::INFINITY, 4), ascii.unknown_bar(4));
    }

    #[test]
    fn meters_keep_their_brackets_only_when_there_is_room_for_a_bar() {
        let ascii = GlyphSet::ascii();
        assert_eq!(ascii.meter(1.0, 5), "[###]");
        assert_eq!(ascii.meter(1.0, 3), "[#]");
        // Two cells would be brackets with nothing inside; show the bar instead.
        assert_eq!(ascii.meter(1.0, 2), "##");
        assert_eq!(ascii.meter(1.0, 1), "#");
        assert_eq!(ascii.meter(1.0, 0), "");
    }

    #[test]
    fn zero_width_never_panics_for_any_renderer() {
        for set in BOTH {
            assert!(set.bar(0.5, 0).is_empty());
            assert!(set.meter(0.5, 0).is_empty());
            assert!(set.unknown_bar(0).is_empty());
            assert!(set.unknown_meter(0).is_empty());
            assert!(set.sparkline(&[1.0, 2.0], 0, 2.0).is_empty());
            assert!(set.dense_sparkline(&[1.0, 2.0], 0, 2.0).is_empty());
        }
    }

    #[test]
    fn dense_mode_packs_two_samples_per_cell_in_enhanced_mode() {
        let unicode = GlyphSet::unicode();
        // Four samples fit in two cells; eight samples still fit in four.
        assert_eq!(
            unicode.dense_sparkline(&[1.0; 4], 8, 1.0).chars().count(),
            8
        );
        let two_cells = unicode.dense_sparkline(&[1.0; 4], 2, 1.0);
        assert_eq!(two_cells.chars().count(), 2);
        assert!(
            two_cells
                .chars()
                .all(|c| ('\u{2800}'..='\u{28ff}').contains(&c))
        );
    }

    #[test]
    fn dense_mode_degrades_to_the_ascii_ramp_in_strict_mode() {
        let ascii = GlyphSet::ascii();
        let samples = [0.0, 25.0, 50.0, 75.0, 100.0];
        assert_eq!(
            ascii.dense_sparkline(&samples, 5, 100.0),
            ascii.sparkline(&samples, 5, 100.0)
        );
    }

    #[test]
    fn a_measured_zero_still_lights_a_braille_dot() {
        // Braille cells are blank only where there is no sample at all.
        let unicode = GlyphSet::unicode();
        let rendered = unicode.dense_sparkline(&[0.0, 0.0], 1, 100.0);
        assert_eq!(rendered, "\u{28c0}");
    }

    #[test]
    fn auto_uses_enhanced_mode_for_a_utf8_locale() {
        let env = TerminalEnv::empty()
            .with_lang("en_US.UTF-8")
            .with_term("xterm-256color");
        assert_eq!(GlyphMode::Auto.resolve(&env), GlyphStyle::Unicode);
        let musl = TerminalEnv::empty()
            .with_lc_all("C.utf8")
            .with_term("screen");
        assert_eq!(GlyphMode::Auto.resolve(&musl), GlyphStyle::Unicode);
        let bare = TerminalEnv::empty()
            .with_lc_ctype("UTF-8")
            .with_term("xterm");
        assert_eq!(GlyphMode::Auto.resolve(&bare), GlyphStyle::Unicode);
    }

    #[test]
    fn auto_falls_back_to_ascii_without_a_utf8_locale() {
        let latin = TerminalEnv::empty()
            .with_lang("en_US.ISO-8859-1")
            .with_term("xterm");
        assert_eq!(GlyphMode::Auto.resolve(&latin), GlyphStyle::Ascii);
        let posix = TerminalEnv::empty().with_lang("C").with_term("xterm");
        assert_eq!(GlyphMode::Auto.resolve(&posix), GlyphStyle::Ascii);
        assert_eq!(
            GlyphMode::Auto.resolve(&TerminalEnv::empty()),
            GlyphStyle::Ascii
        );
    }

    #[test]
    fn auto_falls_back_to_ascii_on_a_dumb_terminal_even_with_utf8() {
        let env = TerminalEnv::empty()
            .with_lang("en_US.UTF-8")
            .with_term("dumb");
        assert_eq!(GlyphMode::Auto.resolve(&env), GlyphStyle::Ascii);
        let unset = TerminalEnv::empty().with_lang("en_US.UTF-8");
        assert_eq!(GlyphMode::Auto.resolve(&unset), GlyphStyle::Ascii);
    }

    #[test]
    fn locale_precedence_follows_posix() {
        let env = TerminalEnv::empty()
            .with_lang("en_US.UTF-8")
            .with_lc_ctype("en_US.UTF-8")
            .with_lc_all("C")
            .with_term("xterm");
        // LC_ALL wins, so the UTF-8 LANG/LC_CTYPE do not apply.
        assert_eq!(GlyphMode::Auto.resolve(&env), GlyphStyle::Ascii);

        let ctype_wins = TerminalEnv::empty()
            .with_lang("C")
            .with_lc_ctype("en_US.UTF-8")
            .with_term("xterm");
        assert_eq!(GlyphMode::Auto.resolve(&ctype_wins), GlyphStyle::Unicode);
    }

    #[test]
    fn an_empty_locale_variable_is_ignored_rather_than_treated_as_set() {
        let env = TerminalEnv::empty()
            .with_lc_all("")
            .with_lang("en_US.UTF-8")
            .with_term("xterm");
        assert_eq!(GlyphMode::Auto.resolve(&env), GlyphStyle::Unicode);
    }

    #[test]
    fn an_explicit_mode_ignores_detection_entirely() {
        let hostile = TerminalEnv::empty().with_lang("C").with_term("dumb");
        assert_eq!(GlyphMode::Unicode.resolve(&hostile), GlyphStyle::Unicode);
        let friendly = TerminalEnv::empty()
            .with_lang("en_US.UTF-8")
            .with_term("xterm-256color");
        assert_eq!(GlyphMode::Ascii.resolve(&friendly), GlyphStyle::Ascii);
    }

    #[test]
    fn glyph_modes_parse_the_documented_cli_spellings() {
        assert_eq!("auto".parse(), Ok(GlyphMode::Auto));
        assert_eq!("unicode".parse(), Ok(GlyphMode::Unicode));
        assert_eq!(" ASCII ".parse(), Ok(GlyphMode::Ascii));
        let err = "utf".parse::<GlyphMode>().expect_err("not a mode");
        assert!(err.to_string().contains("utf"), "{err}");
        for mode in [GlyphMode::Auto, GlyphMode::Unicode, GlyphMode::Ascii] {
            assert_eq!(mode.to_string().parse(), Ok(mode));
        }
    }

    #[test]
    fn no_color_requires_a_non_empty_value() {
        assert!(!TerminalEnv::empty().no_color_requested());
        assert!(!TerminalEnv::empty().with_no_color("").no_color_requested());
        assert!(TerminalEnv::empty().with_no_color("1").no_color_requested());
    }
}

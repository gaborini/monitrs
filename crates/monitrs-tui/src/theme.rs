//! Semantic color tokens and the built-in themes (§5.2, §5.3).
//!
//! Widgets name *meanings*, never colors. That is the whole point of this
//! module: a widget asks for [`Token::Critical`] and the theme decides whether
//! that is `#f7768e`, palette index 204, `Color::LightRed`, or — with color
//! switched off — bold plus underline. §5.3 fixes the token list, so this module
//! adds nothing to it and removes nothing from it.
//!
//! Two invariants are enforced by tests rather than by review:
//!
//! * Meaning survives with zero color. Every token carries a
//!   [`Modifier`] emphasis that applies in all depths, and every
//!   [`Cue`] pairs a token with a character, so nothing depends on color alone.
//! * Nothing animates. There is no state in a [`Theme`] at all, no frame
//!   counter, and no blink modifier anywhere — §5.2 forbids continuously
//!   alternating or flashing colors, and the way to guarantee that is to make it
//!   unrepresentable.

use core::fmt;
use core::str::FromStr;

use monitrs_core::model::{MetricState, PressureState};
use ratatui::style::{Color, Modifier, Style};
use thiserror::Error;

use crate::glyphs::TerminalEnv;

/// The `--color` setting as requested (§5.2).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum ColorMode {
    /// Detect from the environment, honoring `NO_COLOR`.
    #[default]
    Auto,
    /// 24-bit RGB.
    TrueColor,
    /// The 256-color indexed palette.
    Ansi256,
    /// The 16 named ANSI colors.
    Ansi16,
    /// No color at all; only modifiers.
    Off,
}

/// The color capability after detection: there is no `Auto` left to interpret.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ColorDepth {
    /// No color at all; only modifiers.
    Off,
    /// The 16 named ANSI colors.
    Ansi16,
    /// The 256-color indexed palette.
    Ansi256,
    /// 24-bit RGB.
    TrueColor,
}

impl ColorDepth {
    /// Every depth, weakest first.
    pub const ALL: [Self; 4] = [Self::Off, Self::Ansi16, Self::Ansi256, Self::TrueColor];

    /// The depths that can actually express a color.
    pub const COLORED: [Self; 3] = [Self::Ansi16, Self::Ansi256, Self::TrueColor];

    /// Whether any color can be expressed at this depth.
    #[must_use]
    pub const fn has_color(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// The `--color` value was not one of the five documented spellings.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("unknown color mode {value:?}; expected one of auto, truecolor, 256, 16, off")]
pub struct ColorModeParseError {
    /// The rejected input.
    pub value: String,
}

impl FromStr for ColorMode {
    type Err = ColorModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "truecolor" | "24bit" => Ok(Self::TrueColor),
            "256" | "ansi256" => Ok(Self::Ansi256),
            "16" | "ansi16" => Ok(Self::Ansi16),
            "off" | "none" => Ok(Self::Off),
            _ => Err(ColorModeParseError {
                value: value.to_owned(),
            }),
        }
    }
}

impl fmt::Display for ColorMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::TrueColor => "truecolor",
            Self::Ansi256 => "256",
            Self::Ansi16 => "16",
            Self::Off => "off",
        })
    }
}

impl ColorMode {
    /// Resolves to a concrete depth.
    ///
    /// `explicit` must be `true` only when the value came from a `--color` flag
    /// on the command line. §5.2 says `NO_COLOR` is honored *unless an explicit
    /// CLI flag overrides it*, so a color mode that came from a configuration
    /// file loses to `NO_COLOR` while the same value passed as a flag wins.
    /// `Auto` never overrides `NO_COLOR`, because "detect" cannot be a statement
    /// of intent to have color.
    #[must_use]
    pub fn resolve(self, env: &TerminalEnv, explicit: bool) -> ColorDepth {
        if matches!(self, Self::Off) {
            return ColorDepth::Off;
        }
        let overridden = explicit && !matches!(self, Self::Auto);
        if env.no_color_requested() && !overridden {
            return ColorDepth::Off;
        }
        match self {
            Self::TrueColor => ColorDepth::TrueColor,
            Self::Ansi256 => ColorDepth::Ansi256,
            Self::Ansi16 => ColorDepth::Ansi16,
            Self::Off => ColorDepth::Off,
            Self::Auto => Self::detect(env),
        }
    }

    /// Resolves against the real process environment.
    #[must_use]
    pub fn resolve_from_process(self, explicit: bool) -> ColorDepth {
        self.resolve(&TerminalEnv::from_process(), explicit)
    }

    /// Detects the depth from `COLORTERM` and `TERM`.
    ///
    /// `COLORTERM` is checked first because it is the only variable that
    /// positively advertises 24-bit support; `TERM` only ever hints at it.
    fn detect(env: &TerminalEnv) -> ColorDepth {
        if let Some(colorterm) = env.colorterm() {
            let lowered = colorterm.to_ascii_lowercase();
            if lowered.contains("truecolor") || lowered.contains("24bit") {
                return ColorDepth::TrueColor;
            }
        }
        let Some(term) = env.term() else {
            // No terminal type at all: assume the output is not a terminal.
            return ColorDepth::Off;
        };
        let term = term.to_ascii_lowercase();
        if term.is_empty() || term == "dumb" {
            return ColorDepth::Off;
        }
        if term.contains("truecolor") || term.contains("direct") {
            return ColorDepth::TrueColor;
        }
        if term.contains("256color") {
            return ColorDepth::Ansi256;
        }
        ColorDepth::Ansi16
    }
}

/// A semantic palette entry. Exactly the §5.3 list, with nothing added.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Token {
    /// Terminal background.
    Base,
    /// Panel background where supported.
    Surface,
    /// Ordinary foreground text.
    Text,
    /// De-emphasized text: headers, units, secondary figures.
    Muted,
    /// The single highlight color of a screen.
    Accent,
    /// A healthy state.
    Good,
    /// An elevated state worth watching.
    Watch,
    /// A state actively degrading the system.
    Critical,
    /// The selected row's background.
    Selection,
    /// Unfocused panel borders.
    Border,
    /// The focused panel's border.
    FocusBorder,
    /// A retained value that is no longer fresh.
    Stale,
    /// First graph series.
    Graph1,
    /// Second graph series.
    Graph2,
    /// Third graph series.
    Graph3,
    /// Fourth graph series.
    Graph4,
    /// Fifth graph series.
    Graph5,
    /// Sixth graph series.
    Graph6,
}

impl Token {
    /// The number of tokens.
    pub const COUNT: usize = 18;

    /// Every token, in the order §5.3 lists them.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Base,
        Self::Surface,
        Self::Text,
        Self::Muted,
        Self::Accent,
        Self::Good,
        Self::Watch,
        Self::Critical,
        Self::Selection,
        Self::Border,
        Self::FocusBorder,
        Self::Stale,
        Self::Graph1,
        Self::Graph2,
        Self::Graph3,
        Self::Graph4,
        Self::Graph5,
        Self::Graph6,
    ];

    /// The six graph series tokens, in series order.
    pub const GRAPHS: [Self; 6] = [
        Self::Graph1,
        Self::Graph2,
        Self::Graph3,
        Self::Graph4,
        Self::Graph5,
        Self::Graph6,
    ];

    /// A stable position, used only to prove `ALL` is complete. Test-only,
    /// because it exists for that proof and for nothing else.
    #[cfg(test)]
    const fn index(self) -> usize {
        match self {
            Self::Base => 0,
            Self::Surface => 1,
            Self::Text => 2,
            Self::Muted => 3,
            Self::Accent => 4,
            Self::Good => 5,
            Self::Watch => 6,
            Self::Critical => 7,
            Self::Selection => 8,
            Self::Border => 9,
            Self::FocusBorder => 10,
            Self::Stale => 11,
            Self::Graph1 => 12,
            Self::Graph2 => 13,
            Self::Graph3 => 14,
            Self::Graph4 => 15,
            Self::Graph5 => 16,
            Self::Graph6 => 17,
        }
    }

    /// The name used in configuration and in `--help`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Surface => "surface",
            Self::Text => "text",
            Self::Muted => "muted",
            Self::Accent => "accent",
            Self::Good => "good",
            Self::Watch => "watch",
            Self::Critical => "critical",
            Self::Selection => "selection",
            Self::Border => "border",
            Self::FocusBorder => "focus_border",
            Self::Stale => "stale",
            Self::Graph1 => "graph_1",
            Self::Graph2 => "graph_2",
            Self::Graph3 => "graph_3",
            Self::Graph4 => "graph_4",
            Self::Graph5 => "graph_5",
            Self::Graph6 => "graph_6",
        }
    }

    /// Whether this token draws the eye.
    ///
    /// §5.2 forbids more than one accent color in a single numeric row, and
    /// [`Theme::accent_count`] uses this to decide which tokens count. The
    /// neutrals — text, muted, border, the two backgrounds, selection, and stale
    /// — are structural rather than attention-grabbing, so they do not.
    #[must_use]
    pub const fn is_accent(self) -> bool {
        match self {
            Self::Accent
            | Self::Good
            | Self::Watch
            | Self::Critical
            | Self::Graph1
            | Self::Graph2
            | Self::Graph3
            | Self::Graph4
            | Self::Graph5
            | Self::Graph6 => true,
            Self::Base
            | Self::Surface
            | Self::Text
            | Self::Muted
            | Self::Selection
            | Self::Border
            | Self::FocusBorder
            | Self::Stale => false,
        }
    }

    /// The modifier applied at every depth, so the token still means something
    /// with color switched off (§5.2).
    ///
    /// `Watch` and `Critical` must stay distinguishable from each other without
    /// color, which is why `Critical` adds an underline on top of the bold that
    /// `Watch` already has.
    #[must_use]
    pub const fn emphasis(self) -> Modifier {
        match self {
            Self::Muted | Self::Border => Modifier::DIM,
            Self::Accent | Self::Watch | Self::FocusBorder => Modifier::BOLD,
            Self::Critical => Modifier::BOLD.union(Modifier::UNDERLINED),
            Self::Stale => Modifier::DIM.union(Modifier::ITALIC),
            Self::Base
            | Self::Surface
            | Self::Text
            | Self::Good
            | Self::Selection
            | Self::Graph1
            | Self::Graph2
            | Self::Graph3
            | Self::Graph4
            | Self::Graph5
            | Self::Graph6 => Modifier::empty(),
        }
    }
}

/// One token's color at each depth.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TokenColors {
    truecolor: Color,
    ansi256: Color,
    ansi16: Color,
}

impl TokenColors {
    /// `rgb` is `0x00RRGGBB`; `index` is a 256-palette index; `ansi16` is one of
    /// the sixteen named colors.
    const fn new(rgb: u32, index: u8, ansi16: Color) -> Self {
        Self {
            truecolor: Color::from_u32(rgb),
            ansi256: Color::Indexed(index),
            ansi16,
        }
    }

    const fn at(self, depth: ColorDepth) -> Color {
        match depth {
            ColorDepth::TrueColor => self.truecolor,
            ColorDepth::Ansi256 => self.ansi256,
            ColorDepth::Ansi16 => self.ansi16,
            ColorDepth::Off => Color::Reset,
        }
    }
}

/// Which built-in theme is active. Cycled by `t` (§6.2).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum ThemeId {
    /// Light foreground on a dark background.
    #[default]
    DefaultDark,
    /// Dark foreground on a light background.
    DefaultLight,
    /// Maximum separation between foreground, background, and states (§3.1).
    HighContrast,
}

impl ThemeId {
    /// Every built-in theme, in cycling order.
    pub const ALL: [Self; 3] = [Self::DefaultDark, Self::DefaultLight, Self::HighContrast];

    /// The configuration name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::DefaultDark => "default-dark",
            Self::DefaultLight => "default-light",
            Self::HighContrast => "high-contrast",
        }
    }

    /// The theme this identifier names.
    #[must_use]
    pub const fn theme(self) -> &'static Theme {
        match self {
            Self::DefaultDark => &DEFAULT_DARK,
            Self::DefaultLight => &DEFAULT_LIGHT,
            Self::HighContrast => &HIGH_CONTRAST,
        }
    }

    /// The next theme in the cycle, wrapping at the end.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::DefaultDark => Self::DefaultLight,
            Self::DefaultLight => Self::HighContrast,
            Self::HighContrast => Self::DefaultDark,
        }
    }

    /// Looks up a theme by its configuration name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let lowered = name.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|id| id.name() == lowered)
    }
}

impl fmt::Display for ThemeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A complete palette.
///
/// Fields are named rather than indexed so that a theme literal cannot silently
/// swap two tokens, and the internal lookup matches exhaustively so a new token
/// cannot be forgotten.
/// Emphasis is deliberately *not* per-theme. If a theme could add its own
/// modifiers, a theme that boldened everything would collapse `good` and `watch`
/// into the same appearance at [`ColorDepth::Off`], silently destroying the
/// no-color legibility §5.2 requires. Emphasis therefore belongs to the token's
/// meaning ([`Token::emphasis`]) and a theme only chooses colors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    id: ThemeId,
    base: TokenColors,
    surface: TokenColors,
    text: TokenColors,
    muted: TokenColors,
    accent: TokenColors,
    good: TokenColors,
    watch: TokenColors,
    critical: TokenColors,
    selection: TokenColors,
    border: TokenColors,
    focus_border: TokenColors,
    stale: TokenColors,
    graph_1: TokenColors,
    graph_2: TokenColors,
    graph_3: TokenColors,
    graph_4: TokenColors,
    graph_5: TokenColors,
    graph_6: TokenColors,
}

impl Theme {
    /// Which built-in this is.
    #[must_use]
    pub const fn id(&self) -> ThemeId {
        self.id
    }

    /// The theme's configuration name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.id.name()
    }

    const fn colors(&self, token: Token) -> TokenColors {
        match token {
            Token::Base => self.base,
            Token::Surface => self.surface,
            Token::Text => self.text,
            Token::Muted => self.muted,
            Token::Accent => self.accent,
            Token::Good => self.good,
            Token::Watch => self.watch,
            Token::Critical => self.critical,
            Token::Selection => self.selection,
            Token::Border => self.border,
            Token::FocusBorder => self.focus_border,
            Token::Stale => self.stale,
            Token::Graph1 => self.graph_1,
            Token::Graph2 => self.graph_2,
            Token::Graph3 => self.graph_3,
            Token::Graph4 => self.graph_4,
            Token::Graph5 => self.graph_5,
            Token::Graph6 => self.graph_6,
        }
    }

    /// The token's color at `depth`, or [`Color::Reset`] when there is no color.
    #[must_use]
    pub const fn color(&self, token: Token, depth: ColorDepth) -> Color {
        self.colors(token).at(depth)
    }

    /// The token's emphasis at any depth.
    #[must_use]
    pub const fn emphasis(&self, token: Token) -> Modifier {
        token.emphasis()
    }

    /// The style for text drawn in `token`.
    ///
    /// At [`ColorDepth::Off`] the foreground is explicitly reset and only the
    /// emphasis remains, which is what keeps the token's meaning legible with
    /// zero color (§5.2).
    #[must_use]
    pub fn style(&self, token: Token, depth: ColorDepth) -> Style {
        Style::new()
            .fg(self.color(token, depth))
            .add_modifier(self.emphasis(token))
    }

    /// The style for a panel background drawn in `token`.
    ///
    /// Separate from [`Theme::style`] because `base` and `surface` are the only
    /// tokens that name a background, and applying them as a foreground would
    /// make text invisible.
    #[must_use]
    pub fn background_style(&self, token: Token, depth: ColorDepth) -> Style {
        Style::new().bg(self.color(token, depth))
    }

    /// The selected row's style.
    ///
    /// The foreground is [`Token::Text`] over a [`Token::Selection`] background
    /// so that the two are chosen together and cannot drift apart. With color
    /// off, [`Modifier::REVERSED`] swaps the terminal's own foreground and
    /// background, which is the only way to keep the row distinguishable at zero
    /// color (§5.2).
    #[must_use]
    pub fn selection_style(&self, depth: ColorDepth) -> SelectionStyle {
        if depth.has_color() {
            SelectionStyle {
                fg: self.color(Token::Text, depth),
                bg: self.color(Token::Selection, depth),
                modifier: Modifier::empty(),
            }
        } else {
            SelectionStyle {
                fg: Color::Reset,
                bg: Color::Reset,
                modifier: Modifier::REVERSED,
            }
        }
    }

    /// How many distinct accent colors `tokens` would put in one row.
    ///
    /// §5.2 caps this at one for a numeric row. Distinct *colors* are counted
    /// rather than distinct tokens, because at [`ColorDepth::Ansi16`] two
    /// semantically different accents legitimately collapse onto the same ANSI
    /// color, and one color is one distraction.
    #[must_use]
    pub fn accent_count<I: IntoIterator<Item = Token>>(
        &self,
        depth: ColorDepth,
        tokens: I,
    ) -> usize {
        let mut seen: Vec<Color> = Vec::new();
        for token in tokens {
            if !token.is_accent() {
                continue;
            }
            let color = self.color(token, depth);
            if color == Color::Reset {
                // Nothing to distract with at this depth.
                continue;
            }
            if !seen.contains(&color) {
                seen.push(color);
            }
        }
        seen.len()
    }
}

/// The resolved foreground, background, and modifier of the selected row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionStyle {
    /// The selected row's foreground.
    pub fg: Color,
    /// The selected row's background.
    pub bg: Color,
    /// Additional modifiers; carries [`Modifier::REVERSED`] with color off.
    pub modifier: Modifier,
}

impl SelectionStyle {
    /// Whether the selected row is distinguishable from an unselected one.
    ///
    /// True when the foreground and background differ, or when the row is
    /// reversed — reversing exchanges the terminal's default pair, so the two
    /// still differ even though both are [`Color::Reset`].
    #[must_use]
    pub fn is_readable(&self) -> bool {
        self.fg != self.bg || self.modifier.contains(Modifier::REVERSED)
    }

    /// The ratatui style for the row.
    #[must_use]
    pub fn into_style(self) -> Style {
        Style::new()
            .fg(self.fg)
            .bg(self.bg)
            .add_modifier(self.modifier)
    }
}

/// A token paired with the character that carries the same information.
///
/// §5.2 forbids relying on color alone, so anything that colors a value by state
/// must also print the state's symbol. Bundling them makes it impossible to take
/// one without the other, and the symbols come from the frozen `symbol()`
/// methods in `monitrs-core` so that a state reads identically everywhere.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cue {
    /// The color token for this state.
    pub token: Token,
    /// The redundant, non-color character for this state.
    pub symbol: char,
}

impl Cue {
    /// The cue for a metric's availability.
    ///
    /// `Available` maps to plain text with a blank symbol: a value that was
    /// measured normally is not an anomaly and must not be decorated. Everything
    /// else is either de-emphasized (nothing to act on) or flagged as `Watch`
    /// (the read failed and the user may be able to fix it).
    #[must_use]
    pub fn for_metric<T>(state: &MetricState<T>) -> Self {
        let token = match state {
            MetricState::Available(_) => Token::Text,
            MetricState::Stale { .. } => Token::Stale,
            MetricState::WarmingUp | MetricState::Unsupported => Token::Muted,
            MetricState::PermissionDenied | MetricState::TemporarilyUnavailable(_) => Token::Watch,
        };
        Self {
            token,
            symbol: state.symbol(),
        }
    }

    /// The cue for a Pressure Radar state.
    #[must_use]
    pub const fn for_pressure(state: PressureState) -> Self {
        let token = match state {
            PressureState::Normal => Token::Good,
            PressureState::Watch => Token::Watch,
            PressureState::Critical => Token::Critical,
        };
        Self {
            token,
            symbol: state.symbol(),
        }
    }

    /// The cue for a pressure state that may itself be unavailable.
    ///
    /// A signal whose state could not be derived must not read as `normal`, so
    /// the availability cue wins over the (absent) pressure state.
    #[must_use]
    pub fn for_pressure_state(state: &MetricState<PressureState>) -> Self {
        match state.displayable() {
            Some((&pressure, _)) if state.is_available() => Self::for_pressure(pressure),
            // A retained value keeps its state symbol but is drawn as stale.
            Some((&pressure, _)) => Self {
                token: Token::Stale,
                symbol: pressure.symbol(),
            },
            None => Self::for_metric(state),
        }
    }
}

/// Light foreground on a dark background.
const DEFAULT_DARK: Theme = Theme {
    id: ThemeId::DefaultDark,
    base: TokenColors::new(0x0012_141a, 234, Color::Black),
    surface: TokenColors::new(0x0019_1c24, 235, Color::Black),
    text: TokenColors::new(0x00d6_dae3, 252, Color::Gray),
    muted: TokenColors::new(0x007c_8494, 245, Color::DarkGray),
    accent: TokenColors::new(0x007a_a2f7, 111, Color::LightBlue),
    good: TokenColors::new(0x0079_c98f, 114, Color::LightGreen),
    watch: TokenColors::new(0x00e0_af68, 179, Color::LightYellow),
    critical: TokenColors::new(0x00f7_768e, 204, Color::LightRed),
    selection: TokenColors::new(0x0026_3250, 24, Color::Blue),
    border: TokenColors::new(0x003b_4252, 240, Color::DarkGray),
    focus_border: TokenColors::new(0x0056_b6c2, 81, Color::LightCyan),
    stale: TokenColors::new(0x006b_7280, 243, Color::DarkGray),
    graph_1: TokenColors::new(0x007a_a2f7, 111, Color::LightBlue),
    graph_2: TokenColors::new(0x0079_c98f, 114, Color::LightGreen),
    graph_3: TokenColors::new(0x00e0_af68, 179, Color::LightYellow),
    graph_4: TokenColors::new(0x00c7_92ea, 176, Color::LightMagenta),
    graph_5: TokenColors::new(0x0056_b6c2, 80, Color::LightCyan),
    graph_6: TokenColors::new(0x00f7_768e, 204, Color::LightRed),
};

/// Dark foreground on a light background.
const DEFAULT_LIGHT: Theme = Theme {
    id: ThemeId::DefaultLight,
    base: TokenColors::new(0x00fb_fbfd, 231, Color::White),
    surface: TokenColors::new(0x00f1_f2f6, 255, Color::White),
    text: TokenColors::new(0x001f_2430, 236, Color::Black),
    muted: TokenColors::new(0x005c_6370, 244, Color::DarkGray),
    accent: TokenColors::new(0x001f_5fbf, 25, Color::Blue),
    // The indexed and named colors are darker than the hue the true-color value
    // names, and deliberately so. On a light background the accessibility review
    // measured palette index 28 (`#008700`) at 4.05:1 against `surface` and ANSI
    // `Green` at 2.16:1 against `base` — a `normal` label nobody can read. Of the
    // sixteen ANSI names only `Black`, `Blue`, `Red`, and `Magenta` clear 4.5:1 on
    // a light background, so `good` takes `Blue` and `watch` takes `Magenta`
    // rather than the green and yellow that a dark theme can afford (§5.2:
    // information must survive at every depth). See
    // `tests/accessibility.rs::every_theme_meets_the_contrast_floor_it_promises`.
    good: TokenColors::new(0x001f_7a3f, 22, Color::Blue),
    watch: TokenColors::new(0x008a_5a00, 94, Color::Magenta),
    critical: TokenColors::new(0x00a3_162b, 124, Color::Red),
    selection: TokenColors::new(0x00c9_d8f2, 152, Color::Cyan),
    border: TokenColors::new(0x00c3_c7d1, 250, Color::Gray),
    focus_border: TokenColors::new(0x000f_6f7a, 30, Color::LightBlue),
    stale: TokenColors::new(0x006b_7280, 245, Color::DarkGray),
    graph_1: TokenColors::new(0x001f_5fbf, 25, Color::Blue),
    graph_2: TokenColors::new(0x001f_7a3f, 28, Color::Green),
    graph_3: TokenColors::new(0x008a_5a00, 130, Color::Yellow),
    graph_4: TokenColors::new(0x007b_2fa0, 90, Color::Magenta),
    graph_5: TokenColors::new(0x000f_6f7a, 30, Color::LightBlue),
    graph_6: TokenColors::new(0x00a3_162b, 124, Color::Red),
};

/// Maximum separation, for low-vision users and for projectors (§3.1).
const HIGH_CONTRAST: Theme = Theme {
    id: ThemeId::HighContrast,
    base: TokenColors::new(0x0000_0000, 16, Color::Black),
    surface: TokenColors::new(0x0000_0000, 16, Color::Black),
    text: TokenColors::new(0x00ff_ffff, 231, Color::White),
    muted: TokenColors::new(0x00c0_c0c0, 250, Color::Gray),
    accent: TokenColors::new(0x0000_ffff, 51, Color::LightCyan),
    good: TokenColors::new(0x0000_ff00, 46, Color::LightGreen),
    watch: TokenColors::new(0x00ff_ff00, 226, Color::LightYellow),
    critical: TokenColors::new(0x00ff_0000, 196, Color::LightRed),
    // A mid-blue rather than the near-black `#0000c0` this theme used to carry.
    // Blue contributes only 7% of relative luminance, so a saturated dark blue
    // band measured 1.6–2.2:1 against this theme's black `base`: the selected row
    // was a band you could not see in the one theme whose entire purpose is
    // separation. `#5555ff` keeps white text at 5.09:1 *and* lifts the band itself
    // to 4.13:1 against `base`, which is close to the arithmetic best possible for
    // white-on-blue-on-black (both ratios cannot exceed 4.58:1 at once).
    selection: TokenColors::new(0x0055_55ff, 62, Color::LightBlue),
    border: TokenColors::new(0x00ff_ffff, 231, Color::White),
    focus_border: TokenColors::new(0x00ff_ff00, 226, Color::LightYellow),
    stale: TokenColors::new(0x00c0_c0c0, 250, Color::Gray),
    graph_1: TokenColors::new(0x00ff_ffff, 231, Color::White),
    graph_2: TokenColors::new(0x0000_ffff, 51, Color::LightCyan),
    graph_3: TokenColors::new(0x0000_ff00, 46, Color::LightGreen),
    graph_4: TokenColors::new(0x00ff_ff00, 226, Color::LightYellow),
    graph_5: TokenColors::new(0x00ff_00ff, 201, Color::LightMagenta),
    graph_6: TokenColors::new(0x00ff_0000, 196, Color::LightRed),
};

#[cfg(test)]
mod tests {
    use monitrs_core::model::UnavailableReason;

    use super::*;

    fn themes() -> [&'static Theme; 3] {
        [
            ThemeId::DefaultDark.theme(),
            ThemeId::DefaultLight.theme(),
            ThemeId::HighContrast.theme(),
        ]
    }

    #[test]
    fn all_lists_every_token_exactly_once() {
        assert_eq!(Token::ALL.len(), Token::COUNT);
        for (position, token) in Token::ALL.iter().enumerate() {
            assert_eq!(
                token.index(),
                position,
                "{token:?} is missing from or misplaced in Token::ALL"
            );
        }
    }

    #[test]
    fn the_token_list_is_exactly_the_specified_palette() {
        // §5.3 fixes this list; drift in either direction is a spec violation.
        let names: Vec<&str> = Token::ALL.iter().map(|token| token.name()).collect();
        assert_eq!(
            names,
            vec![
                "base",
                "surface",
                "text",
                "muted",
                "accent",
                "good",
                "watch",
                "critical",
                "selection",
                "border",
                "focus_border",
                "stale",
                "graph_1",
                "graph_2",
                "graph_3",
                "graph_4",
                "graph_5",
                "graph_6",
            ]
        );
    }

    #[test]
    fn every_theme_defines_every_token_at_every_depth() {
        for theme in themes() {
            for token in Token::ALL {
                for depth in ColorDepth::COLORED {
                    assert_ne!(
                        theme.color(token, depth),
                        Color::Reset,
                        "{} leaves {} unset at {depth:?}",
                        theme.name(),
                        token.name()
                    );
                }
                assert_eq!(theme.color(token, ColorDepth::Off), Color::Reset);
            }
        }
    }

    #[test]
    fn selected_row_foreground_and_background_differ_in_every_theme_and_mode() {
        for theme in themes() {
            for depth in ColorDepth::ALL {
                let selection = theme.selection_style(depth);
                assert!(
                    selection.is_readable(),
                    "{} at {depth:?} renders an unreadable selection: {selection:?}",
                    theme.name()
                );
            }
            for depth in ColorDepth::COLORED {
                let selection = theme.selection_style(depth);
                assert_ne!(
                    selection.fg,
                    selection.bg,
                    "{} at {depth:?} uses the same colour for both",
                    theme.name()
                );
            }
            // With no color the pair is reset, so reversing is what separates it.
            let off = theme.selection_style(ColorDepth::Off);
            assert!(off.modifier.contains(Modifier::REVERSED));
        }
    }

    #[test]
    fn a_numeric_row_never_carries_more_than_one_accent() {
        // The composition a process row actually uses: neutral figures plus a
        // single state-coloured cell.
        let row = [
            Token::Text,
            Token::Text,
            Token::Critical,
            Token::Muted,
            Token::Text,
            Token::Stale,
            Token::Border,
        ];
        for theme in themes() {
            for depth in ColorDepth::ALL {
                assert!(
                    theme.accent_count(depth, row) <= 1,
                    "{} at {depth:?} counts {} accents",
                    theme.name(),
                    theme.accent_count(depth, row)
                );
            }
        }
    }

    #[test]
    fn the_accent_counter_actually_detects_a_violation() {
        // Otherwise the previous test would pass with a broken counter.
        let bad_row = [Token::Text, Token::Critical, Token::Good, Token::Accent];
        for theme in themes() {
            for depth in ColorDepth::COLORED {
                assert!(
                    theme.accent_count(depth, bad_row) > 1,
                    "{} at {depth:?} failed to flag three accents",
                    theme.name()
                );
            }
            // With color off there is no accent to over-use.
            assert_eq!(theme.accent_count(ColorDepth::Off, bad_row), 0);
        }
    }

    #[test]
    fn accent_counting_ignores_neutral_tokens() {
        let neutrals = [
            Token::Base,
            Token::Surface,
            Token::Text,
            Token::Muted,
            Token::Selection,
            Token::Border,
            Token::FocusBorder,
            Token::Stale,
        ];
        for theme in themes() {
            assert_eq!(theme.accent_count(ColorDepth::TrueColor, neutrals), 0);
        }
    }

    #[test]
    fn good_watch_and_critical_are_distinguishable_at_every_colour_depth() {
        for theme in themes() {
            for depth in ColorDepth::COLORED {
                let good = theme.color(Token::Good, depth);
                let watch = theme.color(Token::Watch, depth);
                let critical = theme.color(Token::Critical, depth);
                assert_ne!(good, watch, "{} at {depth:?}", theme.name());
                assert_ne!(watch, critical, "{} at {depth:?}", theme.name());
                assert_ne!(good, critical, "{} at {depth:?}", theme.name());
            }
        }
    }

    #[test]
    fn states_stay_distinguishable_without_colour_through_modifiers() {
        // §5.2: never rely on red/green alone. With colour off the three states
        // must still differ, and they do so by modifier.
        for theme in themes() {
            let good = theme.style(Token::Good, ColorDepth::Off);
            let watch = theme.style(Token::Watch, ColorDepth::Off);
            let critical = theme.style(Token::Critical, ColorDepth::Off);
            assert_ne!(good, watch, "{}", theme.name());
            assert_ne!(watch, critical, "{}", theme.name());
            assert_ne!(good, critical, "{}", theme.name());
        }
    }

    #[test]
    fn the_six_graph_series_are_distinct_within_every_theme_and_depth() {
        for theme in themes() {
            for depth in ColorDepth::COLORED {
                let mut seen: Vec<Color> = Vec::new();
                for token in Token::GRAPHS {
                    let color = theme.color(token, depth);
                    assert!(
                        !seen.contains(&color),
                        "{} repeats {color:?} at {depth:?}",
                        theme.name()
                    );
                    seen.push(color);
                }
            }
        }
    }

    #[test]
    fn text_is_distinguishable_from_both_backgrounds() {
        for theme in themes() {
            for depth in ColorDepth::COLORED {
                let text = theme.color(Token::Text, depth);
                assert_ne!(text, theme.color(Token::Base, depth), "{}", theme.name());
                assert_ne!(text, theme.color(Token::Surface, depth), "{}", theme.name());
                assert_ne!(text, theme.color(Token::Muted, depth), "{}", theme.name());
            }
        }
    }

    #[test]
    fn no_theme_can_flash_or_alternate() {
        // §5.2 forbids continuously alternating colours. The theme carries no
        // frame counter and no blink modifier, so it cannot.
        for theme in themes() {
            for token in Token::ALL {
                for depth in ColorDepth::ALL {
                    let style = theme.style(token, depth);
                    assert!(
                        !style.add_modifier.contains(Modifier::SLOW_BLINK),
                        "{} {} blinks",
                        theme.name(),
                        token.name()
                    );
                    assert!(
                        !style.add_modifier.contains(Modifier::RAPID_BLINK),
                        "{} {} blinks",
                        theme.name(),
                        token.name()
                    );
                    // Purity: the same request always yields the same style.
                    assert_eq!(style, theme.style(token, depth));
                }
            }
            for depth in ColorDepth::ALL {
                let selection = theme.selection_style(depth);
                assert!(!selection.modifier.contains(Modifier::SLOW_BLINK));
                assert!(!selection.modifier.contains(Modifier::RAPID_BLINK));
            }
        }
    }

    #[test]
    fn colour_off_keeps_meaning_in_the_modifiers() {
        for theme in themes() {
            let critical = theme.style(Token::Critical, ColorDepth::Off);
            assert_eq!(critical.fg, Some(Color::Reset));
            assert!(critical.add_modifier.contains(Modifier::BOLD));
            assert!(critical.add_modifier.contains(Modifier::UNDERLINED));
            let muted = theme.style(Token::Muted, ColorDepth::Off);
            assert!(muted.add_modifier.contains(Modifier::DIM));
            let stale = theme.style(Token::Stale, ColorDepth::Off);
            assert!(stale.add_modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn the_high_contrast_theme_uses_the_extremes_of_the_colour_space() {
        // §3.1 requires a high-contrast built-in. What makes it high contrast is
        // the palette, not extra modifiers: a theme cannot add emphasis, because
        // that would collapse `good` into `watch` with colour off.
        let theme = ThemeId::HighContrast.theme();
        assert_eq!(
            theme.color(Token::Base, ColorDepth::TrueColor),
            Color::Rgb(0, 0, 0)
        );
        assert_eq!(
            theme.color(Token::Text, ColorDepth::TrueColor),
            Color::Rgb(255, 255, 255)
        );
        assert_eq!(
            theme.color(Token::Critical, ColorDepth::TrueColor),
            Color::Rgb(255, 0, 0)
        );
        for token in Token::ALL {
            assert_eq!(
                theme.emphasis(token),
                token.emphasis(),
                "{} gained theme-specific emphasis",
                token.name()
            );
        }
    }

    #[test]
    fn theme_cycling_visits_every_built_in_and_returns_home() {
        let mut id = ThemeId::DefaultDark;
        let mut visited = vec![id];
        for _ in 0..ThemeId::ALL.len() - 1 {
            id = id.next();
            assert!(!visited.contains(&id), "cycle repeats before wrapping");
            visited.push(id);
        }
        assert_eq!(visited.len(), ThemeId::ALL.len());
        assert_eq!(id.next(), ThemeId::DefaultDark);
    }

    #[test]
    fn themes_resolve_by_name_and_report_their_own_name() {
        for id in ThemeId::ALL {
            assert_eq!(ThemeId::from_name(id.name()), Some(id));
            assert_eq!(id.theme().id(), id);
            assert_eq!(id.theme().name(), id.name());
        }
        assert_eq!(
            ThemeId::from_name("  High-Contrast "),
            Some(ThemeId::HighContrast)
        );
        assert_eq!(ThemeId::from_name("solarized"), None);
    }

    #[test]
    fn no_color_is_honored_when_the_mode_is_not_an_explicit_flag() {
        let env = TerminalEnv::empty()
            .with_no_color("1")
            .with_colorterm("truecolor")
            .with_term("xterm-256color");
        assert_eq!(ColorMode::Auto.resolve(&env, false), ColorDepth::Off);
        // `--color auto` is not a statement of intent to have colour.
        assert_eq!(ColorMode::Auto.resolve(&env, true), ColorDepth::Off);
        // A configuration file asking for colour still loses to NO_COLOR.
        assert_eq!(ColorMode::TrueColor.resolve(&env, false), ColorDepth::Off);
        assert_eq!(ColorMode::Ansi16.resolve(&env, false), ColorDepth::Off);
    }

    #[test]
    fn an_explicit_colour_flag_overrides_no_color() {
        let env = TerminalEnv::empty()
            .with_no_color("1")
            .with_term("xterm-256color");
        assert_eq!(
            ColorMode::TrueColor.resolve(&env, true),
            ColorDepth::TrueColor
        );
        assert_eq!(ColorMode::Ansi256.resolve(&env, true), ColorDepth::Ansi256);
        assert_eq!(ColorMode::Ansi16.resolve(&env, true), ColorDepth::Ansi16);
        // `--color off` with NO_COLOR set is still off, not a double negative.
        assert_eq!(ColorMode::Off.resolve(&env, true), ColorDepth::Off);
    }

    #[test]
    fn an_empty_no_color_does_not_disable_colour() {
        let env = TerminalEnv::empty()
            .with_no_color("")
            .with_term("xterm-256color");
        assert_eq!(ColorMode::Auto.resolve(&env, false), ColorDepth::Ansi256);
    }

    #[test]
    fn auto_detection_reads_colorterm_before_term() {
        let env = TerminalEnv::empty()
            .with_colorterm("truecolor")
            .with_term("xterm");
        assert_eq!(ColorMode::Auto.resolve(&env, false), ColorDepth::TrueColor);
        let bit24 = TerminalEnv::empty()
            .with_colorterm("24bit")
            .with_term("screen");
        assert_eq!(
            ColorMode::Auto.resolve(&bit24, false),
            ColorDepth::TrueColor
        );
    }

    #[test]
    fn auto_detection_falls_through_term_to_sixteen_colours() {
        let cases = [
            ("xterm-256color", ColorDepth::Ansi256),
            ("screen-256color", ColorDepth::Ansi256),
            ("xterm-direct", ColorDepth::TrueColor),
            ("xterm", ColorDepth::Ansi16),
            ("vt100", ColorDepth::Ansi16),
            ("dumb", ColorDepth::Off),
            ("", ColorDepth::Off),
        ];
        for (term, expected) in cases {
            let env = TerminalEnv::empty().with_term(term);
            assert_eq!(
                ColorMode::Auto.resolve(&env, false),
                expected,
                "TERM={term:?}"
            );
        }
        // No TERM at all: assume the output is not a terminal.
        assert_eq!(
            ColorMode::Auto.resolve(&TerminalEnv::empty(), false),
            ColorDepth::Off
        );
    }

    #[test]
    fn colour_modes_parse_the_documented_cli_spellings() {
        assert_eq!("auto".parse(), Ok(ColorMode::Auto));
        assert_eq!("truecolor".parse(), Ok(ColorMode::TrueColor));
        assert_eq!("256".parse(), Ok(ColorMode::Ansi256));
        assert_eq!("16".parse(), Ok(ColorMode::Ansi16));
        assert_eq!(" OFF ".parse(), Ok(ColorMode::Off));
        let err = "8".parse::<ColorMode>().expect_err("not a mode");
        assert!(err.to_string().contains('8'), "{err}");
        for mode in [
            ColorMode::Auto,
            ColorMode::TrueColor,
            ColorMode::Ansi256,
            ColorMode::Ansi16,
            ColorMode::Off,
        ] {
            assert_eq!(mode.to_string().parse(), Ok(mode));
        }
    }

    #[test]
    fn every_metric_state_cue_reuses_the_frozen_symbol() {
        let states: [MetricState<u64>; 6] = [
            MetricState::Available(1),
            MetricState::Stale {
                value: 1,
                age: core::time::Duration::from_secs(2),
            },
            MetricState::WarmingUp,
            MetricState::PermissionDenied,
            MetricState::Unsupported,
            MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed),
        ];
        for state in &states {
            let cue = Cue::for_metric(state);
            assert_eq!(cue.symbol, state.symbol(), "{state:?}");
        }
        // Distinct states must remain distinguishable by symbol alone.
        let mut symbols: Vec<char> = states.iter().map(|s| Cue::for_metric(s).symbol).collect();
        symbols.sort_unstable();
        symbols.dedup();
        assert_eq!(symbols.len(), states.len());
    }

    #[test]
    fn every_accent_coloured_cue_also_carries_a_visible_symbol() {
        // §5.2: colour is never the only indicator.
        let states: [MetricState<u64>; 3] = [
            MetricState::PermissionDenied,
            MetricState::TemporarilyUnavailable(UnavailableReason::Timeout),
            MetricState::WarmingUp,
        ];
        for state in &states {
            let cue = Cue::for_metric(state);
            assert!(!cue.symbol.is_whitespace(), "{state:?} has no visible cue");
        }
        for state in [
            PressureState::Normal,
            PressureState::Watch,
            PressureState::Critical,
        ] {
            let cue = Cue::for_pressure(state);
            assert!(cue.token.is_accent());
            assert_eq!(cue.symbol, state.symbol());
            assert!(!cue.symbol.is_whitespace());
        }
    }

    #[test]
    fn a_measured_value_is_not_decorated() {
        let cue = Cue::for_metric(&MetricState::Available(42u64));
        assert_eq!(cue.token, Token::Text);
        assert!(!cue.token.is_accent());
        assert_eq!(cue.symbol, ' ');
    }

    #[test]
    fn an_undetermined_pressure_signal_never_reads_as_normal() {
        let unknown: MetricState<PressureState> =
            MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown);
        let cue = Cue::for_pressure_state(&unknown);
        assert_eq!(cue.symbol, '?');
        assert_ne!(cue.symbol, PressureState::Normal.symbol());
        assert_ne!(cue.token, Token::Good);

        let warming: MetricState<PressureState> = MetricState::WarmingUp;
        assert_eq!(Cue::for_pressure_state(&warming).token, Token::Muted);
    }

    #[test]
    fn a_stale_pressure_signal_keeps_its_state_symbol_but_is_drawn_stale() {
        let stale = MetricState::Available(PressureState::Critical)
            .into_stale(core::time::Duration::from_secs(4));
        let cue = Cue::for_pressure_state(&stale);
        assert_eq!(cue.symbol, 'X');
        assert_eq!(cue.token, Token::Stale);
    }

    #[test]
    fn a_fresh_pressure_signal_uses_its_state_colour() {
        let fresh = MetricState::Available(PressureState::Watch);
        let cue = Cue::for_pressure_state(&fresh);
        assert_eq!(cue.token, Token::Watch);
        assert_eq!(cue.symbol, '!');
    }

    #[test]
    fn background_and_foreground_styles_target_different_slots() {
        let theme = ThemeId::DefaultDark.theme();
        let surface = theme.background_style(Token::Surface, ColorDepth::TrueColor);
        assert_eq!(surface.fg, None);
        assert_eq!(
            surface.bg,
            Some(theme.color(Token::Surface, ColorDepth::TrueColor))
        );
        let text = theme.style(Token::Text, ColorDepth::TrueColor);
        assert_eq!(text.bg, None);
    }

    #[test]
    fn the_selection_style_converts_to_a_ratatui_style() {
        let theme = ThemeId::DefaultLight.theme();
        let selection = theme.selection_style(ColorDepth::Ansi16);
        let style = selection.into_style();
        assert_eq!(style.fg, Some(selection.fg));
        assert_eq!(style.bg, Some(selection.bg));
    }
}

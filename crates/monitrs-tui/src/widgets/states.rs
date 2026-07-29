//! The single place a [`MetricState`] becomes text, a [`Token`], and a symbol.
//!
//! §4's rules — unavailable is never zero, a retained value is visibly marked and
//! carries its age, the first delta sample is warming up rather than `0` — are
//! only worth anything if every widget applies them identically. Nine widgets
//! each writing their own `match` on `MetricState` is nine chances to render
//! `PermissionDenied` as `0%`. So no widget in this crate matches on
//! `MetricState` at all: they all call [`describe`] (or one of its typed
//! wrappers) and render the [`MetricDisplay`] it returns.
//!
//! # Why there are two placeholder widths
//!
//! §4 fixes the placeholder strings (`warming up`, `permission denied`, `n/a`,
//! and the [`UnavailableReason`] messages), and §5.1 fixes `n/a` as the strict
//! ASCII spelling of "no value". A `MEM%` column is five cells wide, so
//! `permission denied` cannot go in it. [`MetricDisplay::fitted`] therefore picks
//! the widest form that fits — the full placeholder in a wide field, `n/a` in a
//! narrow one — and the symbol, which is one cell and always present, is what
//! keeps `permission denied` distinguishable from `warming up` either way (§5.2).
//!
//! [`UnavailableReason`]: monitrs_core::model::UnavailableReason

use core::fmt::Display;
use core::time::Duration;

use monitrs_core::model::{MetricState, PressureState};
use monitrs_core::units::{
    ByteUnits, Ellipsis, Percent, Rate, display_width, format_age, format_byte_rate,
    format_bytes_compact, truncate_tail,
};

use crate::glyphs::{Glyph, GlyphSet};
use crate::theme::{Cue, Token};

/// What one metric looks like on screen: text, a semantic colour, and a symbol.
///
/// The symbol is never optional. §5.2 forbids colour from being the only
/// indicator, and bundling the three together is what makes it impossible to take
/// the colour without the character that carries the same information.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetricDisplay {
    text: String,
    token: Token,
    symbol: char,
    age: Option<Duration>,
    is_value: bool,
}

impl MetricDisplay {
    /// The text to render: a formatted value, or §4's placeholder for the state.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The semantic colour token. Never a literal colour (§5.3).
    #[must_use]
    pub const fn token(&self) -> Token {
        self.token
    }

    /// The redundant, non-colour cue (§5.2).
    ///
    /// `' '` for a freshly measured value: a normal reading is not an anomaly and
    /// must not be decorated.
    #[must_use]
    pub const fn symbol(&self) -> char {
        self.symbol
    }

    /// How long ago a retained value was measured, when this is a stale one.
    ///
    /// `None` for everything else, including fresh values: §4 allows a retained
    /// value on screen *only* alongside its age, so the two travel together.
    #[must_use]
    pub const fn age(&self) -> Option<Duration> {
        self.age
    }

    /// Whether [`MetricDisplay::text`] is a measured value rather than a
    /// placeholder.
    #[must_use]
    pub const fn is_value(&self) -> bool {
        self.is_value
    }

    /// Whether this metric has no value at all (§4).
    #[must_use]
    pub const fn is_placeholder(&self) -> bool {
        !self.is_value
    }

    /// The text plus the stale marker and age, as `71% ~4s`.
    ///
    /// Used where there is room for the mark inline. Where there is not, the
    /// [`Token::Stale`] style and the `~` symbol still carry it.
    #[must_use]
    pub fn annotated(&self) -> String {
        match self.age {
            Some(age) => format!("{} {}{}", self.text, self.symbol, format_age(age)),
            None => self.text.clone(),
        }
    }

    /// The widest form of this metric that fits `width` cells.
    ///
    /// A value is tail-truncated, because a number's leading digits carry the
    /// magnitude. A placeholder degrades to `n/a` and then to the symbol alone
    /// rather than being truncated into something that reads like a different
    /// word — `permission denied` clipped to `permis` is worse than `n/a` (§5.1).
    #[must_use]
    pub fn fitted(&self, width: usize, glyphs: GlyphSet) -> String {
        if width == 0 {
            return String::new();
        }
        if display_width(&self.text) <= width {
            return self.text.clone();
        }
        if self.is_value {
            return fit_within(&self.text, width, glyphs);
        }
        let short = glyphs.unavailable();
        if display_width(short) <= width {
            return short.to_owned();
        }
        // One cell left: the symbol is the whole message. Unlike a value, this is
        // still information — `!` says "the OS refused" all by itself (§5.2).
        self.symbol.to_string()
    }

    /// The symbol followed by the text, as a radar or meter row shows it.
    ///
    /// A measured value's symbol is a space, so this is `" 37%"` when everything
    /// is fine and `"!permission denied"` when it is not — the same shape either
    /// way, which is what stops the row from shifting as states change.
    #[must_use]
    pub fn flagged(&self) -> String {
        format!("{}{}", self.symbol, self.text)
    }
}

/// Describes any [`MetricState`], rendering its value with `render`.
///
/// This is the only function in the crate that matches on [`MetricState`] for
/// display purposes; everything else is a wrapper that supplies `render`.
pub fn describe<T, F: FnOnce(&T) -> String>(state: &MetricState<T>, render: F) -> MetricDisplay {
    let cue = Cue::for_metric(state);
    match state.displayable() {
        Some((value, age)) => MetricDisplay {
            text: render(value),
            token: cue.token,
            symbol: cue.symbol,
            age: if state.is_stale() { Some(age) } else { None },
            is_value: true,
        },
        None => MetricDisplay {
            // A state with no value always has a placeholder; `n/a` is the
            // fallback only so this cannot become an empty cell, which would read
            // as "measured, and nothing there".
            text: state
                .placeholder()
                .unwrap_or(GlyphSet::ascii().unavailable())
                .to_owned(),
            token: cue.token,
            symbol: cue.symbol,
            age: None,
            is_value: false,
        },
    }
}

/// Describes a metric whose value renders through [`Display`].
pub fn describe_display<T: Display>(state: &MetricState<T>) -> MetricDisplay {
    describe(state, ToString::to_string)
}

/// Describes a percentage (§5.4: one decimal only where it adds information).
pub fn describe_percent(state: &MetricState<Percent>) -> MetricDisplay {
    describe(state, ToString::to_string)
}

/// Describes a byte count in the compact column form, such as `2.6G`.
pub fn describe_bytes(state: &MetricState<u64>, units: ByteUnits) -> MetricDisplay {
    describe(state, |bytes| format_bytes_compact(*bytes, units))
}

/// Describes a byte rate with §5.4's consistent `/s` suffix.
pub fn describe_byte_rate(state: &MetricState<Rate>, units: ByteUnits) -> MetricDisplay {
    describe(state, |rate| format_byte_rate(*rate, units))
}

/// Describes a duration in the fixed-width `AGE` column forms.
pub fn describe_age(state: &MetricState<Duration>) -> MetricDisplay {
    describe(state, |age| format_age(*age))
}

/// Describes a pressure state as its lower-case label (§2.3).
///
/// The symbol is deliberately **not** [`MetricState::symbol`] when no state could
/// be derived. §2.3 requires an explicit unavailable state and the radar must
/// never show it the way it shows `normal`; `MetricState::WarmingUp` and
/// `PressureState::Normal` both answer `'.'`, so a signal awaiting samples would
/// otherwise be indistinguishable from a healthy one at a glance. Anything
/// without a derived state therefore gets [`Glyph::StateUnknown`], and the text
/// (`warming up`, `permission denied`, `n/a`) says which kind of unknown it is.
pub fn describe_pressure(state: &MetricState<PressureState>) -> MetricDisplay {
    let cue = Cue::for_pressure_state(state);
    match state.displayable() {
        Some((pressure, age)) => MetricDisplay {
            text: pressure.label().to_owned(),
            token: cue.token,
            symbol: cue.symbol,
            age: if state.is_stale() { Some(age) } else { None },
            is_value: true,
        },
        None => MetricDisplay {
            text: state
                .placeholder()
                .unwrap_or(GlyphSet::ascii().unavailable())
                .to_owned(),
            token: cue.token,
            symbol: unknown_state_symbol(),
            age: None,
            is_value: false,
        },
    }
}

/// The character shown where a state could not be derived at all.
///
/// A free function rather than a literal so the `?` in §5.1's state inventory has
/// exactly one definition, and so it cannot drift away from
/// [`Glyph::StateUnknown`].
#[must_use]
pub fn unknown_state_symbol() -> char {
    // `StateUnknown` is a single character in both glyph modes, asserted by
    // `glyphs::tests::state_characters_match_the_frozen_core_symbols_in_both_modes`.
    GlyphSet::ascii()
        .get(Glyph::StateUnknown)
        .chars()
        .next()
        .unwrap_or('?')
}

/// `text` tail-truncated into `width` cells, or blank when nothing of it survives.
///
/// The blank is the point. `truncate_tail` on its own answers a one-cell budget
/// with `"."` — the first character of `"..."` — and a lone `.` does not read as
/// "there is more here": it reads as data, and specifically as the warming-up
/// symbol or a decimal point. Below the marker's own width the cell is therefore
/// left empty, which is the only rendering that claims nothing.
///
/// This is the rule for *content*. A placeholder degrades differently, via
/// [`MetricDisplay::fitted`], because its symbol still says something at one cell.
#[must_use]
pub fn fit_within(text: &str, width: usize, glyphs: GlyphSet) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }
    let ellipsis = glyphs.ellipsis();
    if width <= ellipsis.width() {
        return String::new();
    }
    truncate_tail(text, width, ellipsis)
}

/// `text` middle-truncated into `width` cells, or blank when nothing survives.
///
/// The middle form is for full command lines and paths, whose two ends both carry
/// information (§5.4). The blank rule is [`fit_within`]'s.
#[must_use]
pub fn fit_middle_within(text: &str, width: usize, glyphs: GlyphSet) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }
    let ellipsis = glyphs.ellipsis();
    if width <= ellipsis.width() {
        return String::new();
    }
    monitrs_core::units::truncate_middle(text, width, ellipsis)
}

/// Right-pads `text` to `width` cells, truncating from the tail if need be.
///
/// A thin wrapper over `monitrs-core` so widgets do not each have to remember to
/// pass the glyph-mode-appropriate ellipsis.
#[must_use]
pub fn pad_left_within(text: &str, width: usize, glyphs: GlyphSet) -> String {
    monitrs_core::units::pad_left(text, width, glyphs.ellipsis())
}

/// Left-pads `text` to `width` cells, for §5.4's right-aligned numeric columns.
#[must_use]
pub fn pad_right_within(text: &str, width: usize, glyphs: GlyphSet) -> String {
    monitrs_core::units::pad_right(text, width, glyphs.ellipsis())
}

/// The ellipsis for a glyph mode, for callers that truncate their own text.
#[must_use]
pub const fn ellipsis_for(glyphs: GlyphSet) -> Ellipsis {
    glyphs.ellipsis()
}

#[cfg(test)]
mod tests {
    use monitrs_core::model::UnavailableReason;

    use super::*;

    const MISSING: [MetricState<Percent>; 4] = [
        MetricState::WarmingUp,
        MetricState::PermissionDenied,
        MetricState::Unsupported,
        MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed),
    ];

    fn percent(value: f32) -> Percent {
        Percent::new(value).expect("a finite non-negative percentage")
    }

    #[test]
    fn an_unavailable_metric_never_renders_as_a_number() {
        for state in MISSING {
            let display = describe_percent(&state);
            assert!(display.is_placeholder(), "{state:?}");
            assert!(
                !display.text().chars().any(|c| c.is_ascii_digit()),
                "{state:?} rendered {:?}",
                display.text()
            );
            assert_ne!(display.text(), "0%", "{state:?}");
            assert!(!display.text().is_empty(), "{state:?}");
        }
    }

    #[test]
    fn warming_up_is_distinguishable_from_a_measured_zero() {
        let warming = describe_percent(&MetricState::WarmingUp);
        let zero = describe_percent(&MetricState::Available(Percent::ZERO));
        assert_eq!(zero.text(), "0%");
        assert_eq!(warming.text(), "warming up");
        assert_ne!(warming.symbol(), zero.symbol());
        assert_ne!(warming.token(), zero.token());
    }

    #[test]
    fn every_state_carries_a_distinct_symbol_so_colour_is_never_needed() {
        let mut symbols = vec![
            describe_percent(&MetricState::Available(percent(1.0))).symbol(),
            describe_percent(
                &MetricState::Available(percent(1.0)).into_stale(Duration::from_secs(2)),
            )
            .symbol(),
        ];
        for state in MISSING {
            symbols.push(describe_percent(&state).symbol());
        }
        let mut unique = symbols.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), symbols.len(), "{symbols:?}");
    }

    #[test]
    fn a_stale_value_is_only_available_together_with_its_age() {
        let state = MetricState::Available(percent(71.0)).into_stale(Duration::from_secs(4));
        let display = describe_percent(&state);
        assert_eq!(display.text(), "71%");
        assert_eq!(display.age(), Some(Duration::from_secs(4)));
        assert_eq!(display.token(), Token::Stale);
        assert_eq!(display.symbol(), '~');
        assert_eq!(display.annotated(), "71% ~00:04");
    }

    #[test]
    fn a_fresh_value_has_no_age_and_no_decoration() {
        let display = describe_percent(&MetricState::Available(percent(37.0)));
        assert_eq!(display.text(), "37%");
        assert_eq!(display.age(), None);
        assert_eq!(display.token(), Token::Text);
        assert_eq!(display.symbol(), ' ');
        assert_eq!(display.annotated(), "37%");
        assert!(
            !display.token().is_accent(),
            "a normal reading is not an alert"
        );
    }

    #[test]
    fn a_placeholder_degrades_to_n_a_and_then_to_its_symbol() {
        let display = describe_percent(&MetricState::PermissionDenied);
        let ascii = GlyphSet::ascii();
        assert_eq!(display.fitted(40, ascii), "permission denied");
        assert_eq!(display.fitted(17, ascii), "permission denied");
        assert_eq!(display.fitted(16, ascii), "n/a");
        assert_eq!(display.fitted(3, ascii), "n/a");
        assert_eq!(display.fitted(2, ascii), "!");
        assert_eq!(display.fitted(1, ascii), "!");
        assert_eq!(display.fitted(0, ascii), "");
    }

    #[test]
    fn a_placeholder_is_never_clipped_into_a_different_word() {
        // `permis` would read as a truncated value rather than as "no value".
        let display = describe_percent(&MetricState::PermissionDenied);
        for width in 1..=16usize {
            let fitted = display.fitted(width, GlyphSet::ascii());
            assert!(
                fitted == "n/a" || fitted == "!",
                "width {width} produced {fitted:?}"
            );
        }
    }

    #[test]
    fn every_fitted_form_respects_its_budget_in_both_glyph_modes() {
        let states: Vec<MetricDisplay> = MISSING
            .iter()
            .map(describe_percent)
            .chain(core::iter::once(describe_percent(&MetricState::Available(
                percent(12_800.0),
            ))))
            .collect();
        for glyphs in [GlyphSet::ascii(), GlyphSet::unicode()] {
            for display in &states {
                for width in 0..=24usize {
                    let fitted = display.fitted(width, glyphs);
                    assert!(
                        display_width(&fitted) <= width,
                        "{:?} at width {width} produced {fitted:?}",
                        display.text()
                    );
                }
            }
        }
    }

    #[test]
    fn a_value_is_tail_truncated_because_the_leading_digits_carry_magnitude() {
        let display = describe_percent(&MetricState::Available(percent(12_800.0)));
        assert_eq!(display.text(), "12800%");
        assert_eq!(display.fitted(6, GlyphSet::ascii()), "12800%");
        assert_eq!(display.fitted(5, GlyphSet::ascii()), "12...");
    }

    #[test]
    fn a_value_too_wide_for_even_a_marker_renders_blank_rather_than_a_bare_dot() {
        // A lone `.` from a clipped `...` reads as data — as the warming-up symbol,
        // or as a decimal point — so a column that cannot keep one digit shows
        // nothing at all.
        let display = describe_percent(&MetricState::Available(percent(12_800.0)));
        for width in 1..=3usize {
            assert_eq!(
                display.fitted(width, GlyphSet::ascii()),
                "",
                "width {width} rendered a marker fragment"
            );
        }
        // Enhanced mode's marker is one cell, so two cells hold a digit and a mark.
        assert_eq!(display.fitted(1, GlyphSet::unicode()), "");
        assert_eq!(display.fitted(2, GlyphSet::unicode()), "1\u{2026}");
    }

    #[test]
    fn a_placeholder_still_speaks_at_one_cell_where_a_value_cannot() {
        // The asymmetry is deliberate: `!` means "the OS refused" on its own, and a
        // truncated number means nothing (§5.2).
        let denied = describe_percent(&MetricState::PermissionDenied);
        assert_eq!(denied.fitted(1, GlyphSet::ascii()), "!");
        let value = describe_percent(&MetricState::Available(percent(12_800.0)));
        assert_eq!(value.fitted(1, GlyphSet::ascii()), "");
    }

    #[test]
    fn fitting_helpers_blank_a_field_too_narrow_to_keep_any_content() {
        let ascii = GlyphSet::ascii();
        assert_eq!(fit_within("rustc", 8, ascii), "rustc");
        assert_eq!(fit_within("rustc-driver", 9, ascii), "rustc-...");
        assert_eq!(fit_within("rustc-driver", 3, ascii), "");
        assert_eq!(
            fit_within("abc", 3, ascii),
            "abc",
            "an exact fit is not truncated"
        );
        assert_eq!(fit_within("", 0, ascii), "");

        let path = "/usr/local/bin/monitrs";
        assert_eq!(fit_middle_within(path, 40, ascii), path);
        let middle = fit_middle_within(path, 14, ascii);
        assert_eq!(
            middle, "/usr/l...nitrs",
            "both ends survive, the middle does not"
        );
        assert_eq!(display_width(&middle), 14);
        assert_eq!(fit_middle_within(path, 3, ascii), "");
    }

    #[test]
    fn the_fitting_helpers_respect_their_budget_with_double_width_text() {
        let name = "\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30d7}\u{30ed}\u{30bb}\u{30b9}";
        for glyphs in [GlyphSet::ascii(), GlyphSet::unicode()] {
            for width in 0..=20usize {
                assert!(display_width(&fit_within(name, width, glyphs)) <= width);
                assert!(display_width(&fit_middle_within(name, width, glyphs)) <= width);
            }
        }
    }

    #[test]
    fn byte_and_rate_forms_use_the_stable_column_spellings() {
        let iec = describe_bytes(
            &MetricState::Available(2 * 1024 * 1024 * 1024),
            ByteUnits::Iec,
        );
        assert_eq!(iec.text(), "2.0G");
        let si = describe_bytes(&MetricState::Available(1000), ByteUnits::Si);
        assert_eq!(si.text(), "1.0K");
        let rate = describe_byte_rate(
            &MetricState::Available(Rate::new(42.0 * 1024.0 * 1024.0).expect("finite")),
            ByteUnits::Iec,
        );
        assert_eq!(rate.text(), "42M/s");
        let denied = describe_byte_rate(&MetricState::PermissionDenied, ByteUnits::Iec);
        assert_eq!(denied.text(), "permission denied");
        assert!(denied.is_placeholder());
    }

    #[test]
    fn an_age_renders_in_the_fixed_width_column_forms() {
        let display = describe_age(&MetricState::Available(Duration::from_secs(43)));
        assert_eq!(display.text(), "00:43");
        let unsupported = describe_age(&MetricState::Unsupported);
        assert_eq!(unsupported.text(), "n/a");
    }

    #[test]
    fn a_derived_pressure_state_shows_its_own_label_and_symbol() {
        for (state, label, symbol) in [
            (PressureState::Normal, "normal", '.'),
            (PressureState::Watch, "watch", '!'),
            (PressureState::Critical, "critical", 'X'),
        ] {
            let display = describe_pressure(&MetricState::Available(state));
            assert_eq!(display.text(), label);
            assert_eq!(display.symbol(), symbol);
            assert!(display.is_value());
        }
    }

    #[test]
    fn an_undetermined_pressure_signal_shows_a_question_mark_never_normal() {
        // §2.3 requires an explicit unavailable state, and `MetricState::WarmingUp`
        // shares `.` with `PressureState::Normal`, so the radar must override it.
        for state in [
            MetricState::WarmingUp,
            MetricState::PermissionDenied,
            MetricState::Unsupported,
            MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown),
        ] {
            let display = describe_pressure(&state);
            assert_eq!(display.symbol(), '?', "{state:?}");
            assert_ne!(
                display.symbol(),
                PressureState::Normal.symbol(),
                "{state:?}"
            );
            assert_ne!(display.text(), "normal", "{state:?}");
            assert_ne!(display.token(), Token::Good, "{state:?}");
        }
    }

    #[test]
    fn a_stale_pressure_signal_keeps_its_state_but_is_marked_stale() {
        let state =
            MetricState::Available(PressureState::Critical).into_stale(Duration::from_secs(9));
        let display = describe_pressure(&state);
        assert_eq!(display.text(), "critical");
        assert_eq!(display.symbol(), 'X');
        assert_eq!(display.token(), Token::Stale);
        assert_eq!(display.age(), Some(Duration::from_secs(9)));
    }

    #[test]
    fn the_flagged_form_has_the_same_shape_whatever_the_state() {
        let available = describe_percent(&MetricState::Available(percent(37.0)));
        let denied = describe_percent(&MetricState::PermissionDenied);
        assert_eq!(available.flagged(), " 37%");
        assert_eq!(denied.flagged(), "!permission denied");
        // One leading cell in both cases, so nothing after it shifts.
        assert_eq!(
            display_width(&available.flagged()) - display_width(available.text()),
            1
        );
        assert_eq!(
            display_width(&denied.flagged()) - display_width(denied.text()),
            1
        );
    }

    #[test]
    fn padding_helpers_respect_their_budget_with_double_width_text() {
        let ascii = GlyphSet::ascii();
        assert_eq!(pad_left_within("37%", 6, ascii), "   37%");
        assert_eq!(pad_right_within("rustc", 8, ascii), "rustc   ");
        assert_eq!(display_width(&pad_left_within("日本語", 5, ascii)), 5);
        assert_eq!(display_width(&pad_right_within("日本語", 5, ascii)), 5);
        assert_eq!(ellipsis_for(GlyphSet::unicode()), Ellipsis::Unicode);
    }

    #[test]
    fn the_unknown_state_symbol_is_the_specified_question_mark() {
        assert_eq!(unknown_state_symbol(), '?');
    }

    #[test]
    fn describe_display_covers_any_printable_value_type() {
        let threads = describe_display(&MetricState::Available(42u32));
        assert_eq!(threads.text(), "42");
        let missing: MetricState<u32> = MetricState::Unsupported;
        assert_eq!(describe_display(&missing).text(), "n/a");
    }
}

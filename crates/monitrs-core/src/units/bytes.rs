//! Byte and byte-rate formatting with stable column widths.
//!
//! §5.4 requires that a value crossing a unit boundary must not reflow the
//! table. Every formatter here therefore produces a string of predictable
//! width: at most 3 significant digits plus an optional decimal, then a
//! fixed-length suffix.

use core::fmt::Write as _;

use super::Rate;

/// Which unit family to render byte counts in.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum ByteUnits {
    /// Powers of 1024: `KiB`, `MiB`, `GiB`. The default (§5.4).
    #[default]
    Iec,
    /// Powers of 1000: `kB`, `MB`, `GB`.
    Si,
}

impl ByteUnits {
    const fn divisor(self) -> u64 {
        match self {
            Self::Iec => 1024,
            Self::Si => 1000,
        }
    }

    /// Suffixes ordered by increasing magnitude, starting at plain bytes.
    const fn suffixes(self) -> &'static [&'static str] {
        match self {
            Self::Iec => &["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"],
            Self::Si => &["B", "kB", "MB", "GB", "TB", "PB", "EB"],
        }
    }

    /// Single-character suffixes for very narrow columns.
    const fn short_suffixes(self) -> &'static [&'static str] {
        match self {
            // Both families collapse to the same letters when abbreviated; the
            // active `ByteUnits` still decides the divisor, and the header
            // states which family is in use.
            Self::Iec | Self::Si => &["B", "K", "M", "G", "T", "P", "E"],
        }
    }
}

/// Selects the largest unit whose value is `>= 1`, returning `(scaled, index)`.
fn scale(bytes: u64, units: ByteUnits, max_index: usize) -> (f64, usize) {
    let divisor = units.divisor();
    let mut value = bytes as f64;
    let mut index = 0usize;
    let divisor_f = divisor as f64;
    while value >= divisor_f && index < max_index {
        value /= divisor_f;
        index += 1;
    }
    (value, index)
}

/// Renders a byte count such as `2.6 GiB`.
///
/// Uses one decimal below 10 and none above, so the digit count never exceeds
/// three and the column width stays stable across unit boundaries.
#[must_use]
pub fn format_bytes(bytes: u64, units: ByteUnits) -> String {
    let suffixes = units.suffixes();
    let (value, index) = scale(bytes, units, suffixes.len().saturating_sub(1));
    let suffix = suffixes.get(index).copied().unwrap_or("B");
    let mut out = String::with_capacity(10);
    if index == 0 {
        // Plain bytes are integral; a decimal would be meaningless.
        let _ = write!(out, "{bytes} {suffix}");
    } else if value < 10.0 {
        let _ = write!(out, "{value:.1} {suffix}");
    } else {
        let _ = write!(out, "{value:.0} {suffix}");
    }
    out
}

/// Renders a byte count in the most compact stable form, such as `2.6G`.
///
/// Used by narrow process-table columns where `2.6 GiB` does not fit.
#[must_use]
pub fn format_bytes_compact(bytes: u64, units: ByteUnits) -> String {
    let suffixes = units.short_suffixes();
    let (value, index) = scale(bytes, units, suffixes.len().saturating_sub(1));
    let suffix = suffixes.get(index).copied().unwrap_or("B");
    let mut out = String::with_capacity(6);
    if index == 0 {
        let _ = write!(out, "{bytes}{suffix}");
    } else if value < 10.0 {
        let _ = write!(out, "{value:.1}{suffix}");
    } else {
        let _ = write!(out, "{value:.0}{suffix}");
    }
    out
}

/// Renders a byte rate with the consistent `/s` suffix required by §5.4.
#[must_use]
pub fn format_byte_rate(rate: Rate, units: ByteUnits) -> String {
    // Rates are validated non-negative and finite, so this truncation is a
    // deliberate floor of an already-bounded value.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let whole = rate.per_second().min(u64::MAX as f64) as u64;
    let mut out = format_bytes_compact(whole, units);
    out.push_str("/s");
    out
}

/// The widest string [`format_bytes_compact`] can produce, for width reservation.
///
/// §5.4 requires reserving column widths from panel geometry rather than from
/// the current value, so layout code needs this bound up front.
pub const MAX_COMPACT_BYTES_WIDTH: u16 = 5;

/// The widest string [`format_byte_rate`] can produce (`999K/s`).
pub const MAX_BYTE_RATE_WIDTH: u16 = MAX_COMPACT_BYTES_WIDTH + 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_bytes_have_no_decimal() {
        assert_eq!(format_bytes(0, ByteUnits::Iec), "0 B");
        assert_eq!(format_bytes(512, ByteUnits::Iec), "512 B");
    }

    #[test]
    fn iec_and_si_boundaries_differ_as_documented() {
        assert_eq!(format_bytes(1024, ByteUnits::Iec), "1.0 KiB");
        assert_eq!(format_bytes(1024, ByteUnits::Si), "1.0 kB");
        assert_eq!(format_bytes(1000, ByteUnits::Si), "1.0 kB");
        assert_eq!(format_bytes(1000, ByteUnits::Iec), "1000 B");
    }

    #[test]
    fn crossing_a_unit_boundary_does_not_widen_the_column() {
        // 1023 B -> 1.0 KiB is the jitter-prone transition called out in §5.4.
        for bytes in [1023u64, 1024, 1025, 1_048_575, 1_048_576] {
            let rendered = format_bytes_compact(bytes, ByteUnits::Iec);
            assert!(
                rendered.chars().count() <= MAX_COMPACT_BYTES_WIDTH as usize,
                "{bytes} rendered as {rendered:?}, wider than the reserved width"
            );
        }
    }

    #[test]
    fn the_largest_counter_still_fits_the_reserved_width() {
        let rendered = format_bytes_compact(u64::MAX, ByteUnits::Iec);
        assert!(
            rendered.chars().count() <= MAX_COMPACT_BYTES_WIDTH as usize,
            "u64::MAX rendered as {rendered:?}"
        );
        assert!(
            rendered.ends_with('E'),
            "expected exbibytes, got {rendered:?}"
        );
    }

    #[test]
    fn rates_carry_the_consistent_per_second_suffix() {
        let rate = Rate::new(42.0 * 1024.0 * 1024.0).expect("valid");
        assert_eq!(format_byte_rate(rate, ByteUnits::Iec), "42M/s");
        assert_eq!(format_byte_rate(Rate::ZERO, ByteUnits::Iec), "0B/s");
    }

    #[test]
    fn every_byte_rate_fits_the_reserved_width() {
        for per_second in [0.0, 1.0, 999.0, 1024.0, 1.5e9, 9.9e18] {
            let rate = Rate::new(per_second).expect("valid");
            let rendered = format_byte_rate(rate, ByteUnits::Iec);
            assert!(
                rendered.chars().count() <= MAX_BYTE_RATE_WIDTH as usize,
                "{per_second} rendered as {rendered:?}"
            );
        }
    }
}

/// Why a byte-size string could not be accepted.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ByteSizeParseError {
    /// The input contained no digits.
    #[error("expected a number followed by a unit (B, KiB, MiB, GiB, kB, MB, GB), got {input:?}")]
    Empty {
        /// The rejected input.
        input: String,
    },
    /// The numeric part was not a valid integer.
    #[error("{value:?} is not a whole number in {input:?}")]
    NotANumber {
        /// The rejected input.
        input: String,
        /// The portion that failed to parse.
        value: String,
    },
    /// The unit suffix was not recognised.
    #[error(
        "unknown size unit {unit:?} in {input:?}; expected one of B, KiB, MiB, GiB, kB, MB, GB"
    )]
    UnknownUnit {
        /// The rejected input.
        input: String,
        /// The unrecognised suffix.
        unit: String,
    },
    /// The value overflowed a `u64`.
    #[error("size {input:?} is too large")]
    Overflow {
        /// The rejected input.
        input: String,
    },
}

/// Parses a byte size such as `32MiB`, `1.5GiB`, `512 kB`, or `1024`.
///
/// Both unit families are accepted regardless of the configured display family,
/// because a configuration file is written by a person who may not know which
/// family is active. IEC and SI suffixes are distinguished by the `i`: `MiB` is
/// 1024², `MB` is 1000². A bare number is bytes — unlike a duration, where a
/// bare number is genuinely ambiguous, "bytes" is the only sensible default here.
///
/// A single decimal fraction is accepted, so anything [`format_bytes`] renders can
/// be pasted back. The fraction is applied with integer arithmetic rather than
/// floating point: `1.5GiB` is exactly 1610612736 bytes, not a value that depends
/// on rounding.
pub fn parse_bytes(input: &str) -> Result<u64, ByteSizeParseError> {
    let trimmed = input.trim();
    let split = trimmed
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit() && *c != '_' && *c != '.')
        .map_or(trimmed.len(), |(index, _)| index);
    let (number, unit) = trimmed.split_at(split);
    let number: String = number.chars().filter(|c| *c != '_').collect();

    let (whole_text, fraction_text) = match number.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (number.as_str(), ""),
    };
    if whole_text.is_empty() || fraction_text.contains('.') {
        return Err(ByteSizeParseError::Empty {
            input: trimmed.to_owned(),
        });
    }
    let amount: u64 = whole_text
        .parse()
        .map_err(|_| ByteSizeParseError::NotANumber {
            input: trimmed.to_owned(),
            value: number.clone(),
        })?;
    let fraction: u64 = if fraction_text.is_empty() {
        0
    } else {
        fraction_text
            .parse()
            .map_err(|_| ByteSizeParseError::NotANumber {
                input: trimmed.to_owned(),
                value: number.clone(),
            })?
    };
    let fraction_scale = 10_u64
        .checked_pow(u32::try_from(fraction_text.len()).unwrap_or(u32::MAX))
        .ok_or(ByteSizeParseError::Overflow {
            input: trimmed.to_owned(),
        })?;

    let unit = unit.trim();
    let multiplier: u64 = match unit {
        "" | "B" | "b" => 1,
        "KiB" | "kib" | "KIB" | "K" | "k" => 1024,
        "MiB" | "mib" | "MIB" | "M" | "m" => 1024 * 1024,
        "GiB" | "gib" | "GIB" | "G" | "g" => 1024 * 1024 * 1024,
        "TiB" | "tib" | "TIB" | "T" | "t" => 1024_u64.pow(4),
        "kB" | "KB" | "kb" => 1_000,
        "MB" | "mB" | "mb" => 1_000_000,
        "GB" | "gB" | "gb" => 1_000_000_000,
        "TB" | "tB" | "tb" => 1_000_000_000_000,
        other => {
            return Err(ByteSizeParseError::UnknownUnit {
                input: trimmed.to_owned(),
                unit: other.to_owned(),
            });
        }
    };

    // u128 intermediate so a large `TiB` value cannot overflow before the
    // fractional part is folded in.
    let total = u128::from(amount) * u128::from(multiplier)
        + u128::from(fraction) * u128::from(multiplier) / u128::from(fraction_scale);
    u64::try_from(total).map_err(|_| ByteSizeParseError::Overflow {
        input: trimmed.to_owned(),
    })
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn parses_both_unit_families() {
        assert_eq!(parse_bytes("32MiB"), Ok(32 * 1024 * 1024));
        assert_eq!(parse_bytes("32MB"), Ok(32_000_000));
        assert_eq!(parse_bytes("1KiB"), Ok(1024));
        assert_eq!(parse_bytes("1kB"), Ok(1_000));
        assert_eq!(parse_bytes("1GiB"), Ok(1024 * 1024 * 1024));
    }

    #[test]
    fn a_single_decimal_fraction_is_exact_integer_arithmetic() {
        assert_eq!(parse_bytes("1.5GiB"), Ok(1_610_612_736));
        assert_eq!(parse_bytes("1.0KiB"), Ok(1024));
        assert_eq!(parse_bytes("0.5MiB"), Ok(512 * 1024));
        assert_eq!(parse_bytes("2.25GiB"), Ok(2_415_919_104));
        // A fraction with no unit is still bytes, floored.
        assert_eq!(parse_bytes("10.9"), Ok(10));
    }

    #[test]
    fn a_malformed_fraction_is_rejected() {
        assert!(parse_bytes("1.2.3MiB").is_err());
        assert!(matches!(
            parse_bytes(".5MiB"),
            Err(ByteSizeParseError::Empty { .. })
        ));
    }

    #[test]
    fn a_bare_number_is_bytes() {
        assert_eq!(parse_bytes("1024"), Ok(1024));
        assert_eq!(parse_bytes("0"), Ok(0));
    }

    #[test]
    fn whitespace_and_underscores_are_tolerated() {
        assert_eq!(parse_bytes(" 32 MiB "), Ok(32 * 1024 * 1024));
        assert_eq!(parse_bytes("1_048_576"), Ok(1_048_576));
    }

    #[test]
    fn the_i_distinguishes_the_families_case_insensitively() {
        assert_eq!(parse_bytes("2MiB"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_bytes("2mib"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_bytes("2MB"), Ok(2_000_000));
        assert_eq!(parse_bytes("2mb"), Ok(2_000_000));
    }

    #[test]
    fn errors_quote_the_offending_input() {
        let error = parse_bytes("32 gigglebytes").expect_err("bad unit");
        assert!(error.to_string().contains("gigglebytes"), "{error}");
        assert!(matches!(
            parse_bytes("MiB"),
            Err(ByteSizeParseError::Empty { .. })
        ));
        assert!(matches!(
            parse_bytes("99999999999999999999999"),
            Err(ByteSizeParseError::NotANumber { .. })
        ));
        assert!(matches!(
            parse_bytes("99999999999999999TiB"),
            Err(ByteSizeParseError::Overflow { .. })
        ));
    }

    #[test]
    fn formatting_round_trips_through_parsing_for_iec_sizes() {
        for bytes in [0u64, 512, 1024, 32 * 1024 * 1024, 4 * 1024 * 1024 * 1024] {
            let rendered = format_bytes(bytes, ByteUnits::Iec).replace(' ', "");
            let reparsed = parse_bytes(&rendered).expect("re-parse");
            // Rendering rounds to at most one decimal, so allow 1% drift; exact
            // powers of the divisor must be exact.
            let drift = reparsed.abs_diff(bytes);
            assert!(
                drift <= bytes / 100 + 1,
                "{bytes} rendered as {rendered} reparsed as {reparsed}"
            );
        }
    }
}

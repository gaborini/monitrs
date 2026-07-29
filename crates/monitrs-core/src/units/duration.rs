//! Duration parsing and display.
//!
//! Parsing is implemented locally rather than pulled from a crate so that the
//! accepted grammar is exactly what the configuration documents, and so that
//! bounds violations can name the offending key (§12).

use core::fmt::Write as _;
use core::time::Duration;

use thiserror::Error;

/// Why a duration string could not be accepted.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DurationParseError {
    /// The input contained no digits.
    #[error("expected a number followed by a unit (ms, s, m, h), got {input:?}")]
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
    #[error("unknown duration unit {unit:?} in {input:?}; expected one of ms, s, m, h")]
    UnknownUnit {
        /// The rejected input.
        input: String,
        /// The unrecognised suffix.
        unit: String,
    },
    /// The value overflowed.
    #[error("duration {input:?} is too large")]
    Overflow {
        /// The rejected input.
        input: String,
    },
}

/// Parses a duration such as `250ms`, `1s`, `5m`, or `1h`.
///
/// A bare number is rejected on purpose: `interval = 1` is ambiguous between
/// one second and one millisecond, and §12 requires that invalid values point at
/// the exact key rather than guess.
pub fn parse_duration(input: &str) -> Result<Duration, DurationParseError> {
    let trimmed = input.trim();
    let split = trimmed
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit() && *c != '_')
        .map_or(trimmed.len(), |(index, _)| index);

    let (digits, unit) = trimmed.split_at(split);
    let digits: String = digits.chars().filter(|c| *c != '_').collect();
    if digits.is_empty() {
        return Err(DurationParseError::Empty {
            input: trimmed.to_owned(),
        });
    }
    let amount: u64 = digits.parse().map_err(|_| DurationParseError::NotANumber {
        input: trimmed.to_owned(),
        value: digits.clone(),
    })?;

    let unit = unit.trim();
    let millis = match unit {
        "ms" => Some(amount),
        "s" | "sec" | "secs" => amount.checked_mul(1_000),
        "m" | "min" | "mins" => amount.checked_mul(60_000),
        "h" | "hr" | "hrs" => amount.checked_mul(3_600_000),
        "" => {
            return Err(DurationParseError::UnknownUnit {
                input: trimmed.to_owned(),
                unit: String::new(),
            });
        }
        other => {
            return Err(DurationParseError::UnknownUnit {
                input: trimmed.to_owned(),
                unit: other.to_owned(),
            });
        }
    };

    millis
        .map(Duration::from_millis)
        .ok_or(DurationParseError::Overflow {
            input: trimmed.to_owned(),
        })
}

/// Renders a duration in the canonical form [`parse_duration`] accepts.
///
/// Used by `config init`, `--help` text, and error messages so that every
/// duration the application prints can be pasted back into configuration.
#[must_use]
pub fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis == 0 {
        return "0ms".to_owned();
    }
    if millis.is_multiple_of(3_600_000) {
        format!("{}h", millis / 3_600_000)
    } else if millis.is_multiple_of(60_000) {
        format!("{}m", millis / 60_000)
    } else if millis.is_multiple_of(1_000) {
        format!("{}s", millis / 1_000)
    } else {
        format!("{millis}ms")
    }
}

/// Renders a process or sample age in the fixed-width forms used by the
/// `AGE` column: `00:43`, `03:12:44`, `12d`.
///
/// The forms are chosen so the column never needs to reflow (§5.4).
#[must_use]
pub fn format_age(age: Duration) -> String {
    let total = age.as_secs();
    let days = total / 86_400;
    let hours = (total % 86_400) / 3_600;
    let minutes = (total % 3_600) / 60;
    let seconds = total % 60;

    let mut out = String::with_capacity(8);
    if days > 0 {
        // Beyond a day, sub-minute precision is noise.
        let _ = write!(out, "{days}d");
    } else if hours > 0 {
        let _ = write!(out, "{hours:02}:{minutes:02}:{seconds:02}");
    } else {
        let _ = write!(out, "{minutes:02}:{seconds:02}");
    }
    out
}

/// Renders a signed offset from live, such as `-00:37`, for the Time Lens
/// header (§2.1).
#[must_use]
pub fn format_history_offset(offset: Duration) -> String {
    if offset.is_zero() {
        return "LIVE".to_owned();
    }
    format!("-{}", format_age(offset))
}

/// Renders a system uptime such as `3d 04:12` (§5.5).
#[must_use]
pub fn format_uptime(uptime: Duration) -> String {
    let total = uptime.as_secs();
    let days = total / 86_400;
    let hours = (total % 86_400) / 3_600;
    let minutes = (total % 3_600) / 60;
    let mut out = String::with_capacity(12);
    if days > 0 {
        let _ = write!(out, "{days}d {hours:02}:{minutes:02}");
    } else {
        let _ = write!(out, "{hours:02}:{minutes:02}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_documented_unit() {
        assert_eq!(parse_duration("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(parse_duration("1s"), Ok(Duration::from_secs(1)));
        assert_eq!(parse_duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_duration("1h"), Ok(Duration::from_secs(3_600)));
        assert_eq!(parse_duration(" 30s "), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("1_000ms"), Ok(Duration::from_secs(1)));
    }

    #[test]
    fn a_bare_number_is_rejected_as_ambiguous() {
        let err = parse_duration("1").expect_err("bare numbers are ambiguous");
        assert!(matches!(err, DurationParseError::UnknownUnit { .. }));
    }

    #[test]
    fn errors_quote_the_offending_input() {
        let err = parse_duration("5 fortnights").expect_err("bad unit");
        let message = err.to_string();
        assert!(message.contains("fortnights"), "{message}");
        assert!(matches!(
            parse_duration("ms"),
            Err(DurationParseError::Empty { .. })
        ));
        assert!(matches!(
            parse_duration("99999999999999999999h"),
            Err(DurationParseError::NotANumber { .. })
        ));
        // Fits in u64 as a number, but overflows when scaled to milliseconds.
        assert!(matches!(
            parse_duration("9999999999999h"),
            Err(DurationParseError::Overflow { .. })
        ));
        // Large but representable: 999999999999h is ~3.6e18 ms, still under u64.
        assert!(parse_duration("999999999999h").is_ok());
    }

    #[test]
    fn formatting_round_trips_through_parsing() {
        for input in ["250ms", "1s", "30s", "5m", "60s", "1h"] {
            let parsed = parse_duration(input).expect("valid");
            let rendered = format_duration(parsed);
            assert_eq!(
                parse_duration(&rendered).expect("re-parse"),
                parsed,
                "{input} rendered as {rendered}"
            );
        }
    }

    #[test]
    fn age_uses_fixed_width_forms() {
        assert_eq!(format_age(Duration::from_secs(43)), "00:43");
        assert_eq!(format_age(Duration::from_secs(138)), "02:18");
        assert_eq!(format_age(Duration::from_secs(11_564)), "03:12:44");
        assert_eq!(format_age(Duration::from_secs(86_400 * 12)), "12d");
    }

    #[test]
    fn history_offset_distinguishes_live_from_a_seek() {
        assert_eq!(format_history_offset(Duration::ZERO), "LIVE");
        assert_eq!(format_history_offset(Duration::from_secs(37)), "-00:37");
    }

    #[test]
    fn uptime_matches_the_header_mockup() {
        assert_eq!(
            format_uptime(Duration::from_secs(86_400 * 3 + 4 * 3_600 + 12 * 60)),
            "3d 04:12"
        );
        assert_eq!(format_uptime(Duration::from_secs(3_600)), "01:00");
    }
}

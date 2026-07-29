//! Byte-slice primitives shared by every `/proc` and `/sys` parser.
//!
//! Two decisions in this file shape the whole Linux layer.
//!
//! **Everything parses `&[u8]`, never a path.** §17.2 requires the parsers to be
//! testable without the live filesystem, and a parser that opens a file cannot be.
//! Keeping the byte-slice boundary here means the parsing is compiled and tested
//! on every platform, including the macOS host this code was written on, and the
//! only Linux-specific part left is the thin reader in [`crate::linux::read`].
//!
//! **A malformed read is a typed failure, not a panic and not a zero.** `/proc`
//! is a kernel ABI, but it is not a stable one: fields have been appended for
//! twenty years, containers virtualise parts of it, and a read of a file the
//! kernel is concurrently updating can return a half-written line. Every function
//! here returns [`ParseFailure`] rather than substituting a number, because §26's
//! *unavailable is not zero* has to hold at the very bottom of the stack or it
//! cannot hold at the top.

use core::time::Duration;

use monitrs_core::model::UnavailableReason;

/// Why a `/proc` or `/sys` parse produced nothing usable.
///
/// Carries a `&'static str` naming the field rather than an owned string, so a
/// failure costs no allocation even when it happens on every process of every
/// tick.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ParseFailure {
    /// The file was empty, or contained only whitespace.
    ///
    /// A real state rather than a corruption: a process that exits while its
    /// `cmdline` is being read yields exactly this.
    Empty,
    /// A field the caller needs was absent from an otherwise readable file.
    ///
    /// Typically an older kernel: `MemAvailable` before 3.14, the discard
    /// counters in `/proc/diskstats` before 4.18.
    Missing(&'static str),
    /// A field was present but did not parse as the kernel documents it.
    Malformed(&'static str),
    /// The file ended in the middle of a record.
    Truncated(&'static str),
}

impl ParseFailure {
    /// The metric state a failed parse produces.
    ///
    /// Always [`UnavailableReason::ParseFailed`]: from the UI's point of view
    /// there is no useful difference between the four failures — the number is
    /// absent either way — while the variants above are what makes a *diagnostic*
    /// line specific enough to act on (§7.5).
    #[must_use]
    pub const fn reason(self) -> UnavailableReason {
        UnavailableReason::ParseFailed
    }

    /// A short human-readable explanation for the diagnostics panel.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::Empty => "file was empty".to_owned(),
            Self::Missing(field) => format!("no {field} field"),
            Self::Malformed(field) => format!("{field} did not parse"),
            Self::Truncated(field) => format!("record ended inside {field}"),
        }
    }
}

/// The result of parsing one `/proc` or `/sys` file.
pub type ParseResult<T> = Result<T, ParseFailure>;

/// Splits a buffer into lines, dropping a trailing newline and ignoring `\r`.
///
/// Yields `&[u8]` rather than `&str` because a `/proc` file may contain a
/// non-UTF-8 process name: a `comm` is arbitrary bytes with only NUL and `/`
/// excluded, so decoding before splitting would either fail or corrupt the line.
pub fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|byte| *byte == b'\n')
        .map(trim_ascii)
        .filter(|line| !line.is_empty())
}

/// Trims ASCII whitespace, including the `\r` of a CRLF line ending.
#[must_use]
pub fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && bytes.get(start).is_some_and(u8::is_ascii_whitespace) {
        start += 1;
    }
    while end > start && bytes.get(end - 1).is_some_and(u8::is_ascii_whitespace) {
        end -= 1;
    }
    bytes.get(start..end).unwrap_or_default()
}

/// Splits on runs of ASCII whitespace, skipping empty fields.
///
/// `/proc/diskstats` and `/proc/net/dev` pad their columns with a variable number
/// of spaces, so a naive `split(b' ')` yields empty fields and shifts every index
/// after the first wide column.
pub fn fields(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty())
}

/// Parses an unsigned decimal integer.
///
/// Rejects a sign, a decimal point, and any trailing text, because a `/proc`
/// counter that is not a bare integer means the format is not the one we think it
/// is — and silently taking the leading digits of `1.5` or `12x` would fabricate
/// a counter (§8.2).
pub fn parse_u64(bytes: &[u8], field: &'static str) -> ParseResult<u64> {
    if bytes.is_empty() {
        return Err(ParseFailure::Missing(field));
    }
    let mut value: u64 = 0;
    for byte in bytes {
        let digit = match byte {
            b'0'..=b'9' => u64::from(byte - b'0'),
            _ => return Err(ParseFailure::Malformed(field)),
        };
        value = value
            .checked_mul(10)
            .and_then(|scaled| scaled.checked_add(digit))
            // A counter wider than u64 is not something to guess at. Saturating
            // would report a near-u64::MAX counter as a real reading and hand the
            // rate engine a bogus baseline.
            .ok_or(ParseFailure::Malformed(field))?;
    }
    Ok(value)
}

/// Parses a signed decimal integer, as used by `nice`, `tpgid`, and
/// `/sys/class/net/*/speed`.
pub fn parse_i64(bytes: &[u8], field: &'static str) -> ParseResult<i64> {
    let (negative, digits) = match bytes.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, bytes),
    };
    let magnitude = parse_u64(digits, field)?;
    let signed = i64::try_from(magnitude).map_err(|_| ParseFailure::Malformed(field))?;
    Ok(if negative { -signed } else { signed })
}

/// Parses a small decimal fraction such as a load average or a PSI percentage.
///
/// Deliberately not `str::parse::<f32>()`: that accepts `inf`, `NaN`, and
/// exponent forms, none of which the kernel ever writes, and all of which would
/// travel straight into a [`monitrs_core::units::Percent`] as a value that is
/// then rejected further up with a less specific reason.
pub fn parse_f32(bytes: &[u8], field: &'static str) -> ParseResult<f32> {
    if bytes.is_empty() {
        return Err(ParseFailure::Missing(field));
    }
    let (negative, rest) = match bytes.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, bytes),
    };
    if rest.is_empty() {
        // A bare sign is not a number.
        return Err(ParseFailure::Malformed(field));
    }
    let mut split = rest.splitn(2, |byte| *byte == b'.');
    let whole_bytes = split.next().unwrap_or_default();
    let whole = if whole_bytes.is_empty() {
        0
    } else {
        parse_u64(whole_bytes, field)?
    };
    let mut value = whole as f32;
    if let Some(fraction_bytes) = split.next() {
        if fraction_bytes.is_empty() {
            return Err(ParseFailure::Malformed(field));
        }
        let fraction = parse_u64(fraction_bytes, field)?;
        let scale = 10f32.powi(i32::try_from(fraction_bytes.len()).unwrap_or(i32::MAX));
        value += fraction as f32 / scale;
    }
    if !value.is_finite() {
        return Err(ParseFailure::Malformed(field));
    }
    Ok(if negative { -value } else { value })
}

/// Decodes bytes as UTF-8, replacing invalid sequences.
///
/// Lossy on purpose: a process name or command-line argument is arbitrary bytes,
/// and refusing to show a row because one argument is not UTF-8 would hide a real
/// process. The replacement character is also what keeps the width calculation in
/// `monitrs_core::units::display_width` honest.
#[must_use]
pub fn to_text(bytes: &[u8]) -> Box<str> {
    String::from_utf8_lossy(bytes).into_owned().into()
}

/// Converts a `key: value` or `key:\tvalue` line into its two halves.
///
/// Used by `/proc/meminfo`, `/proc/<pid>/status`, and `/proc/<pid>/io`, which all
/// share this shape. Returns `None` for a line without a colon, which is how a
/// truncated final line is skipped rather than mis-parsed.
#[must_use]
pub fn split_key_value(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = line.iter().position(|byte| *byte == b':')?;
    let key = trim_ascii(line.get(..colon)?);
    let value = trim_ascii(line.get(colon + 1..)?);
    Some((key, value))
}

/// Converts a count of `USER_HZ` clock ticks into a duration.
///
/// Saturating rather than panicking: a near-`u64::MAX` counter is a fixture case
/// §17.2 requires, and a monitor must not abort on one. `hz` of zero is treated as
/// the standard 100 rather than dividing by zero, and
/// [`crate::linux::DEFAULT_USER_HZ`] documents why 100 is the right default.
#[must_use]
pub fn ticks_to_duration(ticks: u64, hz: u64) -> Duration {
    let hz = if hz == 0 { DEFAULT_TICK_FALLBACK } else { hz };
    let seconds = ticks / hz;
    let remainder = ticks % hz;
    // `remainder < hz`, so this cannot overflow for any plausible hz.
    let nanos = u32::try_from(remainder.saturating_mul(1_000_000_000) / hz).unwrap_or(0);
    Duration::new(seconds, nanos.min(999_999_999))
}

/// The `USER_HZ` value assumed when the caller passes zero.
const DEFAULT_TICK_FALLBACK: u64 = 100;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_runs_do_not_shift_field_indices() {
        // The failure this pins down: `/proc/diskstats` pads its major number to
        // four columns, so splitting on a single space yields three empty fields
        // and every index after it is wrong.
        let line = b"   8       0 sda 150000 3000";
        let parsed: Vec<&[u8]> = fields(line).collect();
        assert_eq!(
            parsed,
            vec![
                &b"8"[..],
                &b"0"[..],
                &b"sda"[..],
                &b"150000"[..],
                &b"3000"[..]
            ]
        );
    }

    #[test]
    fn lines_ignores_blank_lines_and_crlf_endings() {
        let parsed: Vec<&[u8]> = lines(b"a\r\n\nb\n").collect();
        assert_eq!(parsed, vec![&b"a"[..], &b"b"[..]]);
        assert_eq!(lines(b"").count(), 0);
        assert_eq!(lines(b"\n\n\n").count(), 0);
    }

    #[test]
    fn an_unsigned_counter_rejects_anything_but_digits() {
        assert_eq!(parse_u64(b"123", "x"), Ok(123));
        assert_eq!(parse_u64(b"0", "x"), Ok(0));
        assert_eq!(
            parse_u64(b"18446744073709551615", "x"),
            Ok(u64::MAX),
            "a counter at the u64 ceiling is a real reading"
        );
        for bad in [&b"-1"[..], b"1.5", b"12x", b" 12", b"0x10", b""] {
            assert!(parse_u64(bad, "x").is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn a_counter_wider_than_u64_is_a_failure_not_a_saturated_reading() {
        // Saturating would hand the rate engine u64::MAX as a baseline and make
        // the next honest reading look like a counter reset.
        assert_eq!(
            parse_u64(b"18446744073709551616", "x"),
            Err(ParseFailure::Malformed("x"))
        );
        assert_eq!(
            parse_u64(b"999999999999999999999999", "x"),
            Err(ParseFailure::Malformed("x"))
        );
    }

    #[test]
    fn signed_fields_round_trip_including_the_unknown_speed_sentinel() {
        assert_eq!(parse_i64(b"-1", "speed"), Ok(-1));
        assert_eq!(parse_i64(b"1000", "speed"), Ok(1_000));
        assert_eq!(parse_i64(b"+7", "speed"), Ok(7));
        assert!(parse_i64(b"--1", "speed").is_err());
        assert!(
            parse_i64(b"18446744073709551615", "speed").is_err(),
            "a value past i64::MAX must not wrap to a negative"
        );
    }

    #[test]
    fn fractions_parse_without_accepting_nan_or_infinity() {
        assert!((parse_f32(b"11.42", "load").expect("valid") - 11.42).abs() < 0.0001);
        assert!((parse_f32(b"0.00", "load").expect("valid")).abs() < f32::EPSILON);
        assert!((parse_f32(b"7", "load").expect("valid") - 7.0).abs() < f32::EPSILON);
        assert!((parse_f32(b".5", "load").expect("valid") - 0.5).abs() < f32::EPSILON);
        for bad in [&b"nan"[..], b"inf", b"-inf", b"1e10", b"1.", b"1.2.3", b""] {
            assert!(parse_f32(bad, "load").is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn key_value_lines_survive_both_tab_and_space_separators() {
        assert_eq!(
            split_key_value(b"MemTotal:       32784156 kB"),
            Some((&b"MemTotal"[..], &b"32784156 kB"[..]))
        );
        assert_eq!(
            split_key_value(b"Name:\trustc"),
            Some((&b"Name"[..], &b"rustc"[..]))
        );
        assert_eq!(
            split_key_value(b"Buffers:"),
            Some((&b"Buffers"[..], &b""[..])),
            "an empty value is still a recognised key"
        );
        assert_eq!(
            split_key_value(b"MemAvai"),
            None,
            "a truncated final line has no colon and must be skipped"
        );
    }

    #[test]
    fn clock_ticks_convert_without_overflowing_on_an_absurd_counter() {
        assert_eq!(ticks_to_duration(100, 100), Duration::from_secs(1));
        assert_eq!(ticks_to_duration(250, 100), Duration::from_millis(2_500));
        assert_eq!(ticks_to_duration(0, 100), Duration::ZERO);
        // §17.2 requires a very large counter to be handled; the only requirement
        // is that it does not panic and stays monotonic.
        let huge = ticks_to_duration(u64::MAX, 100);
        assert!(huge > ticks_to_duration(u64::MAX - 1_000, 100));
    }

    #[test]
    fn a_zero_clock_rate_falls_back_instead_of_dividing_by_zero() {
        assert_eq!(ticks_to_duration(100, 0), Duration::from_secs(1));
    }

    #[test]
    fn lossy_decoding_keeps_a_non_utf8_process_name_visible() {
        let name = to_text(b"weird\xff\xfe");
        assert!(name.starts_with("weird"));
        assert!(!name.is_empty());
    }

    #[test]
    fn every_failure_explains_itself_without_naming_a_number() {
        for failure in [
            ParseFailure::Empty,
            ParseFailure::Missing("MemAvailable"),
            ParseFailure::Malformed("cpu"),
            ParseFailure::Truncated("stat"),
        ] {
            assert!(!failure.describe().is_empty());
            assert_eq!(failure.reason(), UnavailableReason::ParseFailed);
        }
    }
}

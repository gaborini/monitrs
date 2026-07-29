//! `/proc/pressure/{cpu,memory,io}`: Linux Pressure Stall Information.
//!
//! PSI is the closest thing an operating system offers to "is this resource
//! actually hurting?", which makes it the strongest input the Pressure Radar of
//! §2.3 can have. Each file has one or two lines:
//!
//! ```text
//! some avg10=12.34 avg60=8.10 avg300=3.02 total=1892034821
//! full avg10=1.02 avg60=0.80 avg300=0.31 total=120394821
//! ```
//!
//! `some` is the share of wall time during which at least one task was stalled on
//! the resource; `full` is the share during which *every* non-idle task was.
//!
//! **Many kernels omit the `full` line for CPU** — it was meaningless there before
//! 5.13 and is still absent in containers whose PSI is virtualised — so `full` is a
//! [`MetricState`] on [`PsiResource`] and an absent line becomes
//! [`MetricState::Unsupported`] rather than 0%. A container reporting "0% full CPU
//! pressure" it never measured would be a fabricated all-clear (§4, §26).

use core::time::Duration;

use monitrs_core::model::{MetricState, PsiResource};
use monitrs_core::units::Percent;

use crate::linux::parse::{
    ParseFailure, ParseResult, fields, lines, parse_f32, parse_u64, trim_ascii,
};

/// One `some` or `full` line.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PsiLine {
    avg10: Percent,
    avg60: Percent,
    avg300: Percent,
    total: Duration,
}

/// Parses one `key=value` averaged line.
fn parse_line(fields_after_label: &[u8]) -> ParseResult<PsiLine> {
    let mut avg10 = None;
    let mut avg60 = None;
    let mut avg300 = None;
    let mut total = None;

    for field in fields(fields_after_label) {
        let mut halves = field.splitn(2, |byte| *byte == b'=');
        let key = halves.next().unwrap_or_default();
        let Some(value) = halves.next() else {
            return Err(ParseFailure::Malformed("pressure.field"));
        };
        match key {
            b"avg10" => avg10 = Some(parse_percent(value, "pressure.avg10")?),
            b"avg60" => avg60 = Some(parse_percent(value, "pressure.avg60")?),
            b"avg300" => avg300 = Some(parse_percent(value, "pressure.avg300")?),
            // `total` is microseconds of cumulative stall time, and it is the only
            // monotonic counter in the file — the averages are already derived, so
            // this is what a longer-window rule would have to use.
            b"total" => total = Some(Duration::from_micros(parse_u64(value, "pressure.total")?)),
            _ => {}
        }
    }

    Ok(PsiLine {
        avg10: avg10.ok_or(ParseFailure::Missing("pressure.avg10"))?,
        avg60: avg60.ok_or(ParseFailure::Missing("pressure.avg60"))?,
        avg300: avg300.ok_or(ParseFailure::Missing("pressure.avg300"))?,
        total: total.ok_or(ParseFailure::Missing("pressure.total"))?,
    })
}

/// Parses a PSI average, which the kernel writes as `0.00`–`100.00`.
fn parse_percent(bytes: &[u8], field: &'static str) -> ParseResult<Percent> {
    let value = parse_f32(bytes, field)?;
    Percent::new(value).ok_or(ParseFailure::Malformed(field))
}

/// Parses one `/proc/pressure/*` file.
///
/// The `some` line is required: it is present for every resource on every kernel
/// that has PSI at all, so its absence means the file is not PSI. The `full` line
/// is optional by design.
pub fn parse_pressure(bytes: &[u8]) -> ParseResult<PsiResource> {
    if trim_ascii(bytes).is_empty() {
        return Err(ParseFailure::Empty);
    }
    let mut some: Option<PsiLine> = None;
    let mut full: Option<PsiLine> = None;

    for line in lines(bytes) {
        let mut parts = line.splitn(2, u8::is_ascii_whitespace);
        let label = parts.next().unwrap_or_default();
        let tail = parts.next().unwrap_or_default();
        match label {
            b"some" => some = Some(parse_line(tail)?),
            b"full" => full = Some(parse_line(tail)?),
            _ => {}
        }
    }

    let some = some.ok_or(ParseFailure::Missing("pressure.some"))?;
    let optional = |value: Option<Percent>| match value {
        Some(percent) => MetricState::Available(percent),
        None => MetricState::Unsupported,
    };
    Ok(PsiResource {
        some_avg10: some.avg10,
        some_avg60: some.avg60,
        some_avg300: some.avg300,
        full_avg10: optional(full.map(|line| line.avg10)),
        full_avg60: optional(full.map(|line| line.avg60)),
        full_avg300: optional(full.map(|line| line.avg300)),
        // The `some` total is the one that exists on every resource. Using `full`
        // here would make the counter disappear on exactly the kernels that omit
        // the `full` line.
        total_stalled: some.total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fixtures;

    #[test]
    fn a_file_with_both_lines_yields_some_and_full() {
        let psi = parse_pressure(fixtures::PRESSURE_CPU_WITH_FULL).expect("valid");
        assert!((psi.some_avg10.value() - 12.34).abs() < 0.001);
        assert!((psi.some_avg60.value() - 8.10).abs() < 0.001);
        assert!((psi.some_avg300.value() - 3.02).abs() < 0.001);
        assert!((psi.full_avg10.fresh().expect("present").value() - 1.02).abs() < 0.001);
        assert_eq!(psi.total_stalled, Duration::from_micros(1_892_034_821));
    }

    #[test]
    fn a_kernel_without_the_full_line_reports_unsupported_and_never_zero() {
        // The case §9.2 names explicitly: most kernels omit `full` for CPU.
        let psi = parse_pressure(fixtures::PRESSURE_CPU_WITHOUT_FULL).expect("valid");
        assert!((psi.some_avg10.value() - 12.34).abs() < 0.001);
        assert!(psi.full_avg10.is_unsupported());
        assert!(psi.full_avg60.is_unsupported());
        assert!(psi.full_avg300.is_unsupported());
        assert_ne!(
            psi.full_avg10,
            MetricState::Available(Percent::ZERO),
            "an absent measurement must not read as an all-clear"
        );
        // The cumulative counter still comes from the `some` line.
        assert_eq!(psi.total_stalled, Duration::from_micros(1_892_034_821));
    }

    #[test]
    fn a_genuinely_idle_resource_reports_a_real_zero() {
        // Distinguishing this from the case above is the whole point: 0.00% measured
        // is information, an absent `full` line is not.
        let psi = parse_pressure(fixtures::PRESSURE_IO_IDLE).expect("valid");
        assert_eq!(psi.some_avg10, Percent::ZERO);
        assert_eq!(psi.full_avg10, MetricState::Available(Percent::ZERO));
        assert_eq!(psi.total_stalled, Duration::ZERO);
    }

    #[test]
    fn memory_pressure_parses_with_both_lines_populated() {
        let psi = parse_pressure(fixtures::PRESSURE_MEMORY).expect("valid");
        assert!((psi.some_avg10.value() - 41.20).abs() < 0.001);
        assert!((psi.full_avg300.fresh().expect("present").value() - 9.40).abs() < 0.001);
    }

    #[test]
    fn a_file_without_the_some_line_is_not_psi() {
        assert_eq!(
            parse_pressure(fixtures::PRESSURE_FULL_ONLY),
            Err(ParseFailure::Missing("pressure.some"))
        );
    }

    #[test]
    fn an_empty_file_is_a_typed_failure() {
        assert_eq!(
            parse_pressure(fixtures::PRESSURE_EMPTY),
            Err(ParseFailure::Empty)
        );
        assert_eq!(parse_pressure(b""), Err(ParseFailure::Empty));
    }

    #[test]
    fn a_malformed_average_fails_rather_than_reporting_no_pressure() {
        assert_eq!(
            parse_pressure(fixtures::PRESSURE_MALFORMED),
            Err(ParseFailure::Malformed("pressure.avg10"))
        );
    }

    #[test]
    fn a_missing_average_field_is_reported_as_missing() {
        assert_eq!(
            parse_pressure(b"some avg10=1.00 avg60=1.00 total=5\n"),
            Err(ParseFailure::Missing("pressure.avg300"))
        );
        assert_eq!(
            parse_pressure(b"some avg10=1.00 avg60=1.00 avg300=1.00\n"),
            Err(ParseFailure::Missing("pressure.total"))
        );
    }

    #[test]
    fn a_field_without_an_equals_sign_is_malformed() {
        assert_eq!(
            parse_pressure(b"some avg10 avg60=1.00 avg300=1.00 total=5\n"),
            Err(ParseFailure::Malformed("pressure.field"))
        );
    }

    #[test]
    fn an_average_above_one_hundred_is_kept_rather_than_clamped_away() {
        // PSI cannot exceed 100 by construction, but if a virtualised /proc says
        // otherwise the honest answer is the number it gave us, not a silent clamp
        // that hides a broken source.
        let psi =
            parse_pressure(b"some avg10=120.00 avg60=0.00 avg300=0.00 total=1\n").expect("valid");
        assert!(psi.some_avg10.value() > 100.0);
    }
}

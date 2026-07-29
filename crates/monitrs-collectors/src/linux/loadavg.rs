//! `/proc/loadavg` and `/proc/uptime`.
//!
//! Both are single-line files, and both are read for reasons beyond the obvious
//! one. `/proc/loadavg`'s fourth field is the runnable/total task pair, which is
//! the direct measurement behind the load rule in §11.2 — a load of 11 means
//! something very different with 3 runnable tasks than with 300. `/proc/uptime`
//! turns a process's start time in clock ticks into an age (§7.5) without needing
//! a wall-clock reading at all, which is what keeps §8.1's rule about wall-clock
//! jumps intact.

use core::time::Duration;

use monitrs_core::model::LoadSnapshot;

use crate::linux::parse::{ParseFailure, ParseResult, fields, parse_f32, parse_u64, trim_ascii};

/// The parsed contents of `/proc/loadavg`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoadAvg {
    /// The three exponentially damped run-queue averages.
    pub load: LoadSnapshot,
    /// Tasks currently runnable or running.
    pub runnable: u64,
    /// Total tasks the kernel knows about, including sleeping ones.
    pub total_tasks: u64,
    /// The most recently allocated PID.
    ///
    /// Read because it is the cheapest evidence of PID churn: a jump of thousands
    /// between two ticks means PIDs are being recycled fast enough that identity
    /// really matters (§26).
    pub last_pid: u64,
}

/// Parses `/proc/loadavg`.
///
/// The three averages are required; the task counts and last PID are not, because
/// a container's virtualised `/proc` occasionally truncates the line.
pub fn parse_loadavg(bytes: &[u8]) -> ParseResult<LoadAvg> {
    let trimmed = trim_ascii(bytes);
    if trimmed.is_empty() {
        return Err(ParseFailure::Empty);
    }
    let mut parts = fields(trimmed);
    let mut next_average = |name: &'static str| -> ParseResult<f32> {
        match parts.next() {
            Some(field) => {
                let value = parse_f32(field, name)?;
                if value < 0.0 {
                    // A negative run-queue length is not a small load; it is a
                    // file this parser does not understand.
                    return Err(ParseFailure::Malformed(name));
                }
                Ok(value)
            }
            None => Err(ParseFailure::Truncated(name)),
        }
    };
    let load = LoadSnapshot {
        one: next_average("loadavg.1")?,
        five: next_average("loadavg.5")?,
        fifteen: next_average("loadavg.15")?,
    };

    let (runnable, total_tasks) = match parts.next() {
        Some(pair) => {
            let mut halves = pair.splitn(2, |byte| *byte == b'/');
            let runnable = parse_u64(halves.next().unwrap_or_default(), "loadavg.runnable")?;
            let total = parse_u64(halves.next().unwrap_or_default(), "loadavg.total")?;
            (runnable, total)
        }
        None => (0, 0),
    };
    let last_pid = match parts.next() {
        Some(field) => parse_u64(field, "loadavg.last_pid")?,
        None => 0,
    };

    Ok(LoadAvg {
        load,
        runnable,
        total_tasks,
        last_pid,
    })
}

/// The parsed contents of `/proc/uptime`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Uptime {
    /// Time since boot.
    pub since_boot: Duration,
    /// Summed idle time across every CPU.
    ///
    /// Exceeds `since_boot` on a multi-CPU machine, which is correct and is why it
    /// must never be shown as a share of uptime.
    pub idle: Duration,
}

/// Parses `/proc/uptime`.
///
/// The kernel writes both figures with two decimals, so the fractional part is
/// real information — a process started 40 ms ago has an age, not a zero.
pub fn parse_uptime(bytes: &[u8]) -> ParseResult<Uptime> {
    let trimmed = trim_ascii(bytes);
    if trimmed.is_empty() {
        return Err(ParseFailure::Empty);
    }
    let mut parts = fields(trimmed);
    let Some(uptime_field) = parts.next() else {
        return Err(ParseFailure::Truncated("uptime"));
    };
    let seconds = parse_seconds(uptime_field, "uptime")?;
    let idle = match parts.next() {
        Some(field) => parse_seconds(field, "uptime.idle")?,
        None => Duration::ZERO,
    };
    Ok(Uptime {
        since_boot: seconds,
        idle,
    })
}

/// Parses a `seconds.fraction` field into a duration without losing the fraction.
///
/// Done in integer arithmetic rather than via `f64`: an uptime of a few hundred
/// thousand seconds still has to keep its centiseconds, and going through a float
/// loses precision exactly where process ages are computed from it.
fn parse_seconds(bytes: &[u8], field: &'static str) -> ParseResult<Duration> {
    let mut halves = bytes.splitn(2, |byte| *byte == b'.');
    let whole = parse_u64(halves.next().unwrap_or_default(), field)?;
    let nanos = match halves.next() {
        None => 0,
        // A trailing dot with no digits after it, as in `12.`.
        Some([]) => return Err(ParseFailure::Malformed(field)),
        Some(fraction) => {
            let digits = parse_u64(fraction, field)?;
            let mut scaled = digits;
            let mut places = fraction.len();
            // Normalise to nanoseconds: pad or truncate to nine digits.
            while places < 9 {
                scaled = scaled.saturating_mul(10);
                places += 1;
            }
            while places > 9 {
                scaled /= 10;
                places -= 1;
            }
            u32::try_from(scaled.min(999_999_999)).unwrap_or(0)
        }
    };
    Ok(Duration::new(whole, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fixtures;

    #[test]
    fn a_typical_loadavg_yields_all_five_fields() {
        let parsed = parse_loadavg(fixtures::LOADAVG_TYPICAL).expect("valid");
        assert!((parsed.load.one - 11.42).abs() < 0.001);
        assert!((parsed.load.five - 8.03).abs() < 0.001);
        assert!((parsed.load.fifteen - 4.17).abs() < 0.001);
        assert_eq!(parsed.runnable, 3);
        assert_eq!(parsed.total_tasks, 1_892);
        assert_eq!(parsed.last_pid, 8_123_094);
    }

    #[test]
    fn load_is_a_queue_length_so_it_is_only_comparable_per_cpu() {
        let parsed = parse_loadavg(fixtures::LOADAVG_TYPICAL).expect("valid");
        let per_cpu = parsed.load.per_cpu(8).expect("eight CPUs");
        assert!((per_cpu - 1.4275).abs() < 0.001);
        assert!(
            parsed.load.per_cpu(0).is_none(),
            "undefined without a count"
        );
    }

    #[test]
    fn a_truncated_line_fails_rather_than_reporting_a_zero_load() {
        assert_eq!(
            parse_loadavg(fixtures::LOADAVG_TRUNCATED),
            Err(ParseFailure::Truncated("loadavg.15"))
        );
        assert_eq!(parse_loadavg(b""), Err(ParseFailure::Empty));
    }

    #[test]
    fn a_line_without_the_task_pair_still_yields_the_averages() {
        // Some container runtimes virtualise `/proc/loadavg` down to three fields.
        let parsed = parse_loadavg(b"0.10 0.20 0.30\n").expect("three averages are enough");
        assert!((parsed.load.one - 0.10).abs() < 0.001);
        assert_eq!(parsed.runnable, 0);
    }

    #[test]
    fn a_malformed_average_is_rejected_instead_of_becoming_zero() {
        assert!(parse_loadavg(b"nan 1.0 2.0 1/2 3\n").is_err());
        assert!(parse_loadavg(b"-1.0 1.0 2.0 1/2 3\n").is_err());
        assert!(parse_loadavg(b"1.0 1.0 2.0 3 4\n").is_err(), "no slash");
    }

    #[test]
    fn uptime_keeps_its_fractional_seconds() {
        let parsed = parse_uptime(fixtures::UPTIME_TYPICAL).expect("valid");
        assert_eq!(parsed.since_boot, Duration::new(882_137, 640_000_000));
        assert_eq!(parsed.idle, Duration::new(6_841_203, 110_000_000));
        assert!(
            parsed.idle > parsed.since_boot,
            "summed idle time across CPUs exceeds uptime, which is why it is never \
             shown as a share"
        );
    }

    #[test]
    fn a_malformed_uptime_is_a_typed_failure() {
        assert!(parse_uptime(fixtures::UPTIME_MALFORMED).is_err());
        assert_eq!(parse_uptime(b""), Err(ParseFailure::Empty));
        assert!(parse_uptime(b"12.\n").is_err());
    }

    #[test]
    fn an_uptime_without_the_idle_field_still_parses() {
        let parsed = parse_uptime(b"42.50\n").expect("valid");
        assert_eq!(parsed.since_boot, Duration::new(42, 500_000_000));
        assert_eq!(parsed.idle, Duration::ZERO);
    }

    #[test]
    fn fractions_of_unusual_length_normalise_to_nanoseconds() {
        assert_eq!(
            parse_uptime(b"1.5\n").expect("valid").since_boot,
            Duration::new(1, 500_000_000)
        );
        assert_eq!(
            parse_uptime(b"1.123456789012\n").expect("valid").since_boot,
            Duration::new(1, 123_456_789)
        );
    }
}

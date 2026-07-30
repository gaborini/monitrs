//! Wall-clock formatting for the two overlays that must name a moment.
//!
//! §5.6's Time Lens header reads `sample 22:14:07`, and §6.2 requires the
//! confirmation dialog to show a process's *start time or age*. Both need a
//! [`SystemTime`] rendered for a human, which nothing else in monitrs does: every
//! other duration on screen is relative and comes from
//! [`monitrs_core::units::format_age`] and friends.
//!
//! # Why UTC, and why it says so
//!
//! monitrs has no timezone database and §13 asks for a narrow local implementation
//! rather than a dependency at this scale. Rendering the local time would require
//! one, and rendering UTC *while implying* local time would be a lie about a
//! timestamp the user may compare against a log. So every string this module
//! produces carries `UTC`, and the ambiguity disappears.
//!
//! # This is not a clock read
//!
//! Every function here takes the time it formats. §10.5 forbids the renderer from
//! reading a clock, and a `SystemTime` that came out of a snapshot is data, not a
//! clock: the same snapshot always renders the same string, which is also what makes
//! the snapshot tests deterministic (§17.3).

use std::time::SystemTime;

/// Seconds in a day.
const SECONDS_PER_DAY: i64 = 86_400;

/// Seconds in an hour.
const SECONDS_PER_HOUR: i64 = 3_600;

/// Seconds in a minute.
const SECONDS_PER_MINUTE: i64 = 60;

/// `HH:MM:SS UTC`, the form §5.6 labels a historical sample with.
#[must_use]
pub fn format_time_of_day(time: SystemTime) -> String {
    let (hour, minute, second) = time_of_day(unix_seconds(time));
    format!("{hour:02}:{minute:02}:{second:02} UTC")
}

/// `YYYY-MM-DD HH:MM:SS UTC`, for a moment that may not be today.
///
/// A process started three days ago, so the confirmation dialog cannot show only a
/// time of day: `03:14:07` would read as this morning and the user would confirm a
/// signal against the wrong mental model of which process this is.
#[must_use]
pub fn format_timestamp(time: SystemTime) -> String {
    let seconds = unix_seconds(time);
    let (year, month, day) = civil_from_days(seconds.div_euclid(SECONDS_PER_DAY));
    let (hour, minute, second) = time_of_day(seconds);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// Seconds since the Unix epoch, negative before it.
///
/// A clock set before 1970 is unusual but must not panic, so the error branch is
/// handled rather than unwrapped (§14.3: no panic can be allowed to reach the
/// terminal).
fn unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        Err(error) => i64::try_from(error.duration().as_secs()).map_or(i64::MIN, |secs| -secs),
    }
}

/// Splits Unix seconds into a UTC time of day.
fn time_of_day(seconds: i64) -> (i64, i64, i64) {
    let day = seconds.rem_euclid(SECONDS_PER_DAY);
    (
        day / SECONDS_PER_HOUR,
        (day % SECONDS_PER_HOUR) / SECONDS_PER_MINUTE,
        day % SECONDS_PER_MINUTE,
    )
}

/// Converts days since the Unix epoch into a proleptic Gregorian date.
///
/// The standard shift-the-epoch-to-March algorithm: moving the year boundary to
/// 1 March puts the leap day at the end of the cycle, so leap years and century
/// rules need no special case and no lookup table.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let shifted = days_since_epoch + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    }
    .div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * shifted_month + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    })
    .unwrap_or(1);
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::*;

    fn at(unix_seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(unix_seconds)
    }

    #[test]
    fn the_epoch_renders_as_midnight_on_the_first_of_january_nineteen_seventy() {
        assert_eq!(format_timestamp(at(0)), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_time_of_day(at(0)), "00:00:00 UTC");
    }

    #[test]
    fn a_known_instant_renders_correctly() {
        // 2026-07-29T22:14:07Z, the moment §5.6's mockup is written around.
        assert_eq!(
            format_timestamp(at(1_785_363_247)),
            "2026-07-29 22:14:07 UTC"
        );
        assert_eq!(format_time_of_day(at(1_785_363_247)), "22:14:07 UTC");
    }

    #[test]
    fn leap_days_and_century_rules_are_handled() {
        // 2000 is a leap year, 1900 is not; 2024-02-29 exists.
        assert_eq!(format_timestamp(at(951_782_400)), "2000-02-29 00:00:00 UTC");
        assert_eq!(
            format_timestamp(at(1_709_164_800)),
            "2024-02-29 00:00:00 UTC"
        );
        assert_eq!(
            format_timestamp(at(1_709_251_200)),
            "2024-03-01 00:00:00 UTC"
        );
    }

    #[test]
    fn every_rendering_names_its_timezone() {
        for seconds in [0, 1, 1_785_363_247, 4_102_444_800] {
            assert!(format_timestamp(at(seconds)).ends_with(" UTC"));
            assert!(format_time_of_day(at(seconds)).ends_with(" UTC"));
        }
    }

    #[test]
    fn a_clock_set_before_the_epoch_does_not_panic() {
        let before = SystemTime::UNIX_EPOCH - Duration::from_secs(86_400);
        let rendered = format_timestamp(before);
        assert!(rendered.ends_with(" UTC"), "{rendered}");
        assert_eq!(rendered, "1969-12-31 00:00:00 UTC");
    }

    #[test]
    fn a_clock_far_in_the_future_does_not_panic() {
        // 9999-12-31T23:59:59Z: the last date a four-digit year can express, which is
        // where a naive `{year:04}` would start producing nonsense.
        let far = at(253_402_300_799);
        assert_eq!(format_timestamp(far), "9999-12-31 23:59:59 UTC");
        assert_eq!(format_time_of_day(far), "23:59:59 UTC");
    }

    #[test]
    fn the_time_of_day_wraps_at_midnight_rather_than_overflowing() {
        assert_eq!(format_time_of_day(at(86_399)), "23:59:59 UTC");
        assert_eq!(format_time_of_day(at(86_400)), "00:00:00 UTC");
    }
}

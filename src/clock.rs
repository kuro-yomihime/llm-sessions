//! The clock a person is reading.
//!
//! Stores record UTC and `--json` keeps emitting it, but nobody reading a
//! transcript at 15:53 wants to be told the turn happened at 13:45. Display
//! converts through the machine's offset, captured once at startup: `time`
//! only answers the offset question soundly while the process is still
//! single-threaded, and every scan after that spawns threads.

use std::sync::OnceLock;

use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, Time, UtcOffset};

static LOCAL: OnceLock<UtcOffset> = OnceLock::new();

/// Capture the machine's UTC offset. Call once from `main`, before the first
/// thread; a platform that refuses to answer degrades to UTC.
///
/// The cell is a [`OnceLock`] and not a `LazyLock` on purpose: the answer
/// depends on *when* it is taken. `time` refuses to read the local offset
/// once the process is multi-threaded, and the remote cache keeps a poll
/// thread alive for the whole session, so a lazily-initialised cell would
/// quietly settle on UTC on first draw — and would hand tests the
/// developer's own timezone instead of a fixed one.
pub fn init() {
    let _ = LOCAL.set(UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC));
}

/// The captured offset, or UTC when [`init`] never ran — which is what tests
/// want, so their expectations do not move with the developer's timezone.
pub fn offset() -> UtcOffset {
    LOCAL.get().copied().unwrap_or(UtcOffset::UTC)
}

/// The same instant, read off the given clock.
pub fn at(t: OffsetDateTime, offset: UtcOffset) -> OffsetDateTime {
    t.to_offset(offset)
}
/// Parse `yesterday`, `today`, `<number>d`, `<number>h`, `<number>w`, or an
/// RFC3339 timestamp, relative to `now`.
pub fn parse_since(spec: &str, now: OffsetDateTime) -> Option<OffsetDateTime> {
    let day_start = now
        .date()
        .with_time(Time::MIDNIGHT)
        .assume_offset(now.offset());
    match spec {
        "today" => Some(day_start),
        "yesterday" => day_start.checked_sub(Duration::days(1)),
        _ => relative_since(spec, now).or_else(|| OffsetDateTime::parse(spec, &Rfc3339).ok()),
    }
}

fn relative_since(spec: &str, now: OffsetDateTime) -> Option<OffsetDateTime> {
    let (number, unit) = spec.split_at(spec.len().checked_sub(1)?);
    let number = number.parse::<i64>().ok()?;
    if number < 0 {
        return None;
    }
    let duration = match unit {
        "d" => Duration::days(number),
        "h" => Duration::hours(number),
        "w" => Duration::weeks(number),
        _ => return None,
    };
    now.checked_sub(duration)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_since_accepts_named_relative_and_rfc3339_specs() {
        let now = OffsetDateTime::parse("2026-09-10T15:30:00Z", &Rfc3339).unwrap();
        let midnight = OffsetDateTime::parse("2026-09-10T00:00:00Z", &Rfc3339).unwrap();

        assert_eq!(parse_since("today", now), Some(midnight));
        assert_eq!(
            parse_since("yesterday", now),
            Some(midnight - Duration::days(1))
        );
        assert_eq!(parse_since("7d", now), Some(now - Duration::days(7)));
        assert_eq!(parse_since("24h", now), Some(now - Duration::hours(24)));
        assert_eq!(parse_since("2w", now), Some(now - Duration::weeks(2)));
        assert_eq!(
            parse_since("2026-09-01T12:00:00Z", now),
            OffsetDateTime::parse("2026-09-01T12:00:00Z", &Rfc3339).ok()
        );
        assert_eq!(parse_since("later", now), None);
    }

    #[test]
    fn a_stored_instant_is_read_off_the_local_clock() {
        let utc = time::macros::datetime!(2026 - 09 - 21 23:30 UTC);
        let ahead = at(utc, UtcOffset::from_hms(2, 0, 0).expect("offset"));
        assert_eq!(ahead.date().to_string(), "2026-09-22");
        assert_eq!(ahead.time().as_hms().0, 1);
        // Same instant, so ordering and filtering are untouched.
        assert_eq!(ahead, utc);
    }

    #[test]
    fn an_uninitialised_clock_is_utc() {
        assert_eq!(offset(), UtcOffset::UTC);
    }
}

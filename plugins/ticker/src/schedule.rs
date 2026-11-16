//! When ticks are due: a fixed interval or a cron pattern, as wall-clock
//! instants. Pure functions of time; the clock and the waiting are in `ticker.rs`.

use croner::Cron;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use std::time::Duration;

/// Shortest interval.
pub(crate) const MIN_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone)]
pub(crate) enum Schedule {
    /// `anchor + n × period` for every whole `n` (also before the anchor).
    Every { period: Duration, anchor: Timestamp },
    /// The pattern's matches, in `tz`'s wall clock.
    Cron { cron: Box<Cron>, tz: TimeZone },
}

impl Schedule {
    /// The first tick at or after `t` (`inclusive`) or strictly after it.
    /// `None` if there's no further match (a cron pattern can end, e.g. with a year).
    pub(crate) fn next(&self, t: Timestamp, inclusive: bool) -> Option<Timestamp> {
        match self {
            Schedule::Every { period, anchor } => {
                let period = period.as_nanos() as i128;
                let since = t.as_nanosecond() - anchor.as_nanosecond();
                let mut n = since.div_euclid(period);
                if !(inclusive && since.rem_euclid(period) == 0) {
                    n += 1;
                }
                Timestamp::from_nanosecond(anchor.as_nanosecond() + n * period).ok()
            }
            Schedule::Cron { cron, tz } => cron
                .find_next_occurrence(&t.to_zoned(tz.clone()), inclusive)
                .ok()
                .map(|z| z.timestamp()),
        }
    }

    /// How many ticks fall in `[from, to]`, counting at most `limit`; also the
    /// last of them. `from` must itself be a tick.
    pub(crate) fn count(
        &self,
        from: Timestamp,
        to: Timestamp,
        limit: u64,
    ) -> (u64, Option<Timestamp>) {
        if to < from {
            return (0, None);
        }
        match self {
            Schedule::Every { period, .. } => {
                let period = period.as_nanos() as i128;
                let n = (to.as_nanosecond() - from.as_nanosecond()) / period;
                let last = Timestamp::from_nanosecond(from.as_nanosecond() + n * period).ok();
                ((n as u64).saturating_add(1).min(limit), last)
            }
            Schedule::Cron { .. } => {
                let (mut n, mut last, mut t) = (0u64, None, Some(from));
                while let Some(tick) = t.filter(|t| *t <= to) {
                    n += 1;
                    last = Some(tick);
                    if n >= limit {
                        break;
                    }
                    t = self.next(tick, false);
                }
                (n, last)
            }
        }
    }
}

/// Parses a cron pattern: 5 fields, or 6 with seconds first (also `@hourly`
/// and the other nicknames).
pub(crate) fn parse_cron(pattern: &str) -> Result<Cron, String> {
    croner::parser::CronParser::builder()
        .seconds(croner::parser::Seconds::Optional)
        .build()
        .parse(pattern)
        .map_err(|e| format!("cron {pattern:?}: {e}"))
}

/// Parses a duration: whole numbers with units `ms`, `s`, `m`, `h`, `d`,
/// one or more of them (`500ms`, `10s`, `1h30m`).
pub(crate) fn parse_duration(text: &str) -> Result<Duration, String> {
    let invalid = || format!("{text:?} isn't a duration (e.g. 500ms, 10s, 5m, 1h30m, 1d)");
    let mut rest = text.trim();
    if rest.is_empty() {
        return Err(invalid());
    }
    let mut total = Duration::ZERO;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err(invalid());
        }
        let n: u64 = rest[..digits].parse().map_err(|_| invalid())?;
        rest = &rest[digits..];
        let unit = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
        let millis = match &rest[..unit] {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            _ => return Err(invalid()),
        };
        rest = &rest[unit..];
        let part = n.checked_mul(millis).ok_or_else(invalid)?;
        total = total
            .checked_add(Duration::from_millis(part))
            .ok_or_else(invalid)?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn every(period: &str, anchor: &str) -> Schedule {
        Schedule::Every {
            period: parse_duration(period).unwrap(),
            anchor: ts(anchor),
        }
    }

    fn cron(pattern: &str, tz: &str) -> Schedule {
        Schedule::Cron {
            cron: Box::new(parse_cron(pattern).unwrap()),
            tz: TimeZone::get(tz).unwrap(),
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86_400));
        for bad in [
            "",
            "10",
            "s",
            "1x",
            "1.5s",
            "-1s",
            "1 s",
            "99999999999999999999d",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn interval_ticks_are_anchored() {
        let s = every("10s", "2026-01-01T00:00:00Z");
        assert_eq!(
            s.next(ts("2026-01-01T00:00:00Z"), true),
            Some(ts("2026-01-01T00:00:00Z"))
        );
        assert_eq!(
            s.next(ts("2026-01-01T00:00:00Z"), false),
            Some(ts("2026-01-01T00:00:10Z"))
        );
        assert_eq!(
            s.next(ts("2026-01-01T00:00:03.5Z"), true),
            Some(ts("2026-01-01T00:00:10Z"))
        );
        // Before the anchor: still on the same grid.
        assert_eq!(
            s.next(ts("2025-12-31T23:59:55Z"), false),
            Some(ts("2026-01-01T00:00:00Z"))
        );
        assert_eq!(
            s.count(ts("2026-01-01T00:00:00Z"), ts("2026-01-01T00:00:59Z"), 100),
            (6, Some(ts("2026-01-01T00:00:50Z")))
        );
        assert_eq!(
            s.count(ts("2026-01-01T00:00:00Z"), ts("2026-01-02T00:00:00Z"), 100)
                .0,
            100
        );
    }

    #[test]
    fn cron_with_and_without_seconds() {
        let s = cron("*/15 * * * * *", "UTC");
        assert_eq!(
            s.next(ts("2026-01-01T00:00:01Z"), false),
            Some(ts("2026-01-01T00:00:15Z"))
        );
        let s = cron("30 9 * * MON-FRI", "UTC");
        // 2026-01-03 is a Saturday.
        assert_eq!(
            s.next(ts("2026-01-03T00:00:00Z"), false),
            Some(ts("2026-01-05T09:30:00Z"))
        );
        assert_eq!(
            s.count(ts("2026-01-05T09:30:00Z"), ts("2026-01-09T23:00:00Z"), 100),
            (5, Some(ts("2026-01-09T09:30:00Z")))
        );
        assert!(parse_cron("not cron").is_err());
    }

    #[test]
    fn cron_in_a_time_zone() {
        let s = cron("0 9 * * *", "Europe/Berlin");
        assert_eq!(
            s.next(ts("2026-01-01T00:00:00Z"), false),
            Some(ts("2026-01-01T08:00:00Z"))
        );
        assert_eq!(
            s.next(ts("2026-07-01T00:00:00Z"), false),
            Some(ts("2026-07-01T07:00:00Z"))
        );
    }

    /// The rules the README states for daylight-saving changes (croner's).
    /// Europe/Berlin: 2026-03-29 02:00 → 03:00, 2026-10-25 03:00 → 02:00.
    #[test]
    fn daylight_saving_changes() {
        // A fixed time in the skipped hour runs at the end of the gap (03:00 CEST).
        let s = cron("30 2 * * *", "Europe/Berlin");
        assert_eq!(
            s.next(ts("2026-03-28T12:00:00Z"), false),
            Some(ts("2026-03-29T01:00:00Z"))
        );
        // A fixed time in the repeated hour runs once, at its first instance.
        assert_eq!(
            s.next(ts("2026-10-24T12:00:00Z"), false),
            Some(ts("2026-10-25T00:30:00Z"))
        );
        assert_eq!(
            s.next(ts("2026-10-25T00:30:00Z"), false),
            Some(ts("2026-10-26T01:30:00Z"))
        );
        // An interval pattern skips the missing times and runs in both
        // instances of the repeated hour.
        let s = cron("*/30 * * * *", "Europe/Berlin");
        assert_eq!(
            s.count(ts("2026-03-29T00:00:00Z"), ts("2026-03-29T01:59:00Z"), 100)
                .0,
            4
        );
        assert_eq!(
            s.count(ts("2026-10-25T00:00:00Z"), ts("2026-10-25T01:59:00Z"), 100)
                .0,
            4
        );
        assert_eq!(
            s.next(ts("2026-10-25T00:30:00Z"), false),
            Some(ts("2026-10-25T01:00:00Z"))
        );
    }
}

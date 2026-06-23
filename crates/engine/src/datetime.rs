//! Minimal date/time helpers for the stage-6C function pack. Twill has no native
//! temporal type (spec 16): a timestamp is ISO-8601 `Text` or epoch-seconds
//! `Integer`, and these pure functions parse / format / derive over that model —
//! no new crate (rust.md: keep deps minimal). All arithmetic is UTC; a trailing
//! `Z` or timezone offset is accepted on input and ignored (treated as UTC).

/// Days since the Unix epoch for a civil (proleptic Gregorian) date — Howard
/// Hinnant's `days_from_civil`, valid across a very wide year range.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`]: `(year, month, day)` for a day count.
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Broken-down UTC time: `(year, month, day, hour, min, sec)`.
pub fn parts(epoch: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = epoch.div_euclid(86_400);
    let rem = epoch.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Parse `YYYY-MM-DD[ T]HH:MM:SS[.fff][Z|±HH:MM]` (or a bare date) to epoch
/// seconds (UTC). Returns `None` if the leading `YYYY-MM-DD` is malformed.
pub fn parse_epoch_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut secs = days_from_civil(y, mo, d) * 86_400;
    if let Some(t) = time {
        // Strip a trailing timezone marker (treated as UTC) and fractional secs.
        let t = t.trim_end_matches('Z');
        let t = t.split(['+', 'Z']).next().unwrap_or(t);
        // A '-' in the time part is a tz offset separator (the date already used
        // its '-'), so cut there too.
        let core = t.split('-').next().unwrap_or(t);
        let core = core.split('.').next().unwrap_or(core);
        let mut tp = core.split(':');
        let hh: i64 = tp.next().unwrap_or("0").trim().parse().ok()?;
        let mm: i64 = tp.next().unwrap_or("0").parse().ok()?;
        let ss: i64 = tp.next().unwrap_or("0").parse().ok()?;
        secs += hh * 3600 + mm * 60 + ss;
    }
    Some(secs)
}

/// Format epoch seconds as `YYYY-MM-DD HH:MM:SS` (UTC).
pub fn format_timestamp(epoch: i64) -> String {
    let (y, m, d, hh, mm, ss) = parts(epoch);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
}

/// Format epoch seconds as `YYYY-MM-DD` (UTC).
pub fn format_date(epoch: i64) -> String {
    let (y, m, d, ..) = parts(epoch);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Format epoch seconds as `HH:MM:SS` (UTC).
pub fn format_time(epoch: i64) -> String {
    let (.., hh, mm, ss) = parts(epoch);
    format!("{hh:02}:{mm:02}:{ss:02}")
}

/// Current wall-clock time as epoch seconds (UTC).
pub fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Day-of-week, Sunday = 0 … Saturday = 6 (Postgres `extract(dow …)`).
pub fn day_of_week(epoch: i64) -> i64 {
    let days = epoch.div_euclid(86_400);
    (days.rem_euclid(7) + 4) % 7 // 1970-01-01 was a Thursday (=4)
}

/// Day-of-year, 1-based (Postgres `extract(doy …)`).
pub fn day_of_year(epoch: i64) -> i64 {
    let (y, ..) = parts(epoch);
    let days = epoch.div_euclid(86_400);
    days - days_from_civil(y, 1, 1) + 1
}

/// Truncate epoch seconds to the start of `unit` (UTC); `None` for an unknown
/// unit. Supports year/quarter/month/week/day/hour/minute/second.
pub fn date_trunc(unit: &str, epoch: i64) -> Option<i64> {
    let (y, m, d, hh, mm, _ss) = parts(epoch);
    let at = |y, m, d, hh: i64, mm: i64, ss: i64| {
        days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss
    };
    Some(match unit.to_ascii_lowercase().as_str() {
        "year" => at(y, 1, 1, 0, 0, 0),
        "quarter" => at(y, ((m - 1) / 3) * 3 + 1, 1, 0, 0, 0),
        "month" => at(y, m, 1, 0, 0, 0),
        "week" => {
            // ISO week starts Monday; back up to the most recent Monday.
            let days = epoch.div_euclid(86_400);
            let monday = days - (days.rem_euclid(7) + 3) % 7;
            monday * 86_400
        }
        "day" => at(y, m, d, 0, 0, 0),
        "hour" => at(y, m, d, hh, 0, 0),
        "minute" => at(y, m, d, hh, mm, 0),
        "second" => epoch,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_epoch_zero() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00:00");
        assert_eq!(parse_epoch_secs("1970-01-01 00:00:00"), Some(0));
        assert_eq!(parse_epoch_secs("1970-01-01"), Some(0));
    }

    #[test]
    fn parses_iso_with_tz_and_fraction() {
        let e = parse_epoch_secs("2021-07-04T12:30:45.123Z").unwrap();
        assert_eq!(format_timestamp(e), "2021-07-04 12:30:45");
        assert_eq!(parse_epoch_secs("2021-07-04 12:30:45+02:00"), Some(e));
    }

    #[test]
    fn dow_and_trunc() {
        // 2021-07-04 is a Sunday → dow 0.
        let e = parse_epoch_secs("2021-07-04 09:00:00").unwrap();
        assert_eq!(day_of_week(e), 0);
        assert_eq!(format_date(date_trunc("month", e).unwrap()), "2021-07-01");
        assert_eq!(
            format_timestamp(date_trunc("day", e).unwrap()),
            "2021-07-04 00:00:00"
        );
    }
}

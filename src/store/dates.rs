//! Dates: the number the index holds, and the many ways one may be written.

use super::*;

/// The date forms OpenSearch's default `strict_date_optional_time` accepts.
///
/// A bare `2024-08-12` is a date to OpenSearch but not to RFC 3339, and a
/// field indexed as text rather than as a date has no column for a range or an
/// aggregation to read.
pub fn parse_date_lenient(s: &str) -> Option<boostcore::time::OffsetDateTime> {
    use boostcore::time::{Date, Month, OffsetDateTime, Time};
    if let Some(dt) = crate::query::parse_datetime(s) {
        return Some(dt.into_utc());
    }
    if s.contains("||") || s.starts_with("now") {
        return parse_date_math(s).map(|(dt, _)| dt);
    }
    let (day_part, time_part) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t.trim_end_matches('Z'))),
        None => (s, None),
    };
    let nums: Vec<&str> = day_part.split('-').collect();
    if nums.is_empty() || nums.len() > 3 {
        return None;
    }
    let widths = [4usize, 2, 2];
    let mut parts = [1i64, 1, 1];
    for (i, p) in nums.iter().enumerate() {
        if p.len() != widths[i] || !p.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        parts[i] = p.parse().ok()?;
    }
    let date = Date::from_calendar_date(
        parts[0] as i32,
        Month::try_from(parts[1] as u8).ok()?,
        parts[2] as u8,
    )
    .ok()?;
    let time = match time_part {
        None => Time::MIDNIGHT,
        Some(t) => {
            let (hms, frac) = match t.split_once('.') {
                Some((a, b)) => (a, b),
                None => (t, ""),
            };
            let f: Vec<&str> = hms.split(':').collect();
            if f.is_empty() || f.len() > 3 {
                return None;
            }
            let mut c = [0u32; 3];
            for (i, p) in f.iter().enumerate() {
                c[i] = p.parse().ok()?;
            }
            // the fraction is kept whole: a date reports milliseconds and
            // a date_nanos the nanoseconds, and which one this is has not
            // been decided yet here
            let nanos: u32 = if frac.is_empty() {
                0
            } else {
                let mut d = frac.trim_end_matches(|c: char| !c.is_ascii_digit()).to_string();
                d.truncate(9);
                while d.len() < 9 {
                    d.push('0');
                }
                d.parse().ok()?
            };
            Time::from_hms_nano(c[0] as u8, c[1] as u8, c[2] as u8, nanos).ok()?
        }
    };
    Some(OffsetDateTime::new_utc(date, time))
}

/// Write a date the way a Java-style pattern asks for.
/// A moment written the way a named or literal date format asks for.
pub fn format_millis(ms: i64, format: &str) -> Option<String> {
    format_millis_at(ms, format, 0)
}

/// The same, written in a zone rather than in UTC.
pub fn format_millis_at(ms: i64, format: &str, zone_ms: i64) -> Option<String> {
    if zone_ms != 0 {
        let local = boostcore::time::OffsetDateTime::from_unix_timestamp_nanos(
            (ms + zone_ms) as i128 * 1_000_000,
        )
        .ok()?;
        let total = zone_ms / 60_000;
        let sign = if total < 0 { '-' } else { '+' };
        let total = total.abs();
        let body = match format {
            "iso8601"
            | "strict_date_optional_time"
            | "date_optional_time"
            | "date_time"
            | "strict_date_time" => format!(
                "{}.{:03}",
                format_with_pattern(local, "yyyy-MM-dd'T'HH:mm:ss").replace('\'', ""),
                local.millisecond()
            ),
            other => return format_millis_utc(ms + zone_ms, other),
        };
        return Some(format!("{body}{sign}{:02}:{:02}", total / 60, total % 60));
    }
    format_millis_utc(ms, format)
}

pub(crate) fn format_with_pattern(d: boostcore::time::OffsetDateTime, pattern: &str) -> String {
    let mut out = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        let mut run = 1;
        while chars.peek() == Some(&c) {
            chars.next();
            run += 1;
        }
        match c {
            'y' => out.push_str(&format!("{:0run$}", d.year(), run = run)),
            'M' => out.push_str(&format!("{:0run$}", d.month() as u8, run = run)),
            'd' => out.push_str(&format!("{:0run$}", d.day(), run = run)),
            'H' => out.push_str(&format!("{:0run$}", d.hour(), run = run)),
            'm' => out.push_str(&format!("{:0run$}", d.minute(), run = run)),
            's' => out.push_str(&format!("{:0run$}", d.second(), run = run)),
            other => {
                for _ in 0..run {
                    out.push(other);
                }
            }
        }
    }
    out
}

pub(crate) fn parse_date_math(s: &str) -> Option<(boostcore::time::OffsetDateTime, Option<char>)> {
    use boostcore::time::{Duration, OffsetDateTime};
    let (anchor, ops) = match s.split_once("||") {
        Some((a, o)) => (parse_date_lenient(a)?, o),
        None => (OffsetDateTime::now_utc(), s.strip_prefix("now")?),
    };
    let mut dt = anchor;
    let mut rounded = None;
    let mut rest = ops;
    while !rest.is_empty() {
        let (op, tail) = rest.split_at(1);
        match op {
            "/" => {
                let (unit, tail) = tail.split_at(1.min(tail.len()));
                dt = round_down(dt, unit)?;
                rounded = unit.chars().next();
                rest = tail;
            }
            "+" | "-" => {
                let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
                let tail = &tail[digits.len()..];
                let (unit, tail) = tail.split_at(1.min(tail.len()));
                let n: i64 = if digits.is_empty() { 1 } else { digits.parse().ok()? };
                let n = if op == "-" { -n } else { n };
                dt = match unit {
                    "y" => shift_months(dt, n * 12)?,
                    "M" => shift_months(dt, n)?,
                    "w" => dt + Duration::days(n * 7),
                    "d" => dt + Duration::days(n),
                    "H" | "h" => dt + Duration::hours(n),
                    "m" => dt + Duration::minutes(n),
                    "s" => dt + Duration::seconds(n),
                    _ => return None,
                };
                rest = tail;
            }
            _ => return None,
        }
    }
    Some((dt, rounded))
}

pub(crate) fn advance_unit(
    dt: boostcore::time::OffsetDateTime,
    unit: char,
) -> Option<boostcore::time::OffsetDateTime> {
    use boostcore::time::Duration;
    Some(match unit {
        'y' => shift_months(dt, 12)?,
        'M' => shift_months(dt, 1)?,
        'w' => dt + Duration::days(7),
        'd' => dt + Duration::days(1),
        'H' | 'h' => dt + Duration::hours(1),
        'm' => dt + Duration::minutes(1),
        's' => dt + Duration::seconds(1),
        _ => return None,
    })
}

pub(crate) fn round_down(
    dt: boostcore::time::OffsetDateTime,
    unit: &str,
) -> Option<boostcore::time::OffsetDateTime> {
    use boostcore::time::{Date, Duration, Month, Time};
    let midnight = |d: Date| d.with_time(Time::MIDNIGHT).assume_utc();
    Some(match unit {
        "y" => midnight(Date::from_calendar_date(dt.year(), Month::January, 1).ok()?),
        "M" => midnight(Date::from_calendar_date(dt.year(), dt.month(), 1).ok()?),
        "w" => {
            let back = dt.weekday().number_days_from_monday() as i64;
            midnight(dt.date() - Duration::days(back))
        }
        "d" => midnight(dt.date()),
        "H" | "h" => {
            dt.replace_minute(0).ok()?.replace_second(0).ok()?.replace_nanosecond(0).ok()?
        }
        "m" => dt.replace_second(0).ok()?.replace_nanosecond(0).ok()?,
        "s" => dt.replace_nanosecond(0).ok()?,
        _ => return None,
    })
}

pub(crate) fn shift_months(
    dt: boostcore::time::OffsetDateTime,
    n: i64,
) -> Option<boostcore::time::OffsetDateTime> {
    use boostcore::time::{Date, Month};
    let total = dt.year() as i64 * 12 + (dt.month() as i64 - 1) + n;
    let (y, m) = (total.div_euclid(12) as i32, total.rem_euclid(12) as u8 + 1);
    let month = Month::try_from(m).ok()?;
    let day = dt.day().min(days_in_month(y, month));
    Some(Date::from_calendar_date(y, month, day).ok()?.with_time(dt.time()).assume_utc())
}

/// A date in the one spelling the index holds.
pub fn canonical_date(v: &Value) -> Option<String> {
    canonical_date_with(v, None)
}

/// As `canonical_date`, but honouring the `format` a mapping declares.
///
/// A bare number is epoch milliseconds unless the field says otherwise, which
/// is the assumption OpenSearch makes too.
pub fn canonical_date_with(v: &Value, format: Option<&str>) -> Option<String> {
    canonical_date_prec(v, format, false)
}

/// A date bound as the number the index holds, rounding date math up where the
/// bound is the end of the range it names.
pub fn date_number_bound(
    v: &Value,
    round_up: bool,
    format: Option<&str>,
    nanos: bool,
) -> Option<i64> {
    // `gte: 2019` on a date field is the year, not two seconds past the epoch:
    // the default format reads a bare four-digit number as a year, and nothing
    // sane asks for a bound two seconds into 1970
    if format.is_none() {
        if let Some(year) = v.as_i64().filter(|n| (1000..=9999).contains(n)) {
            return date_number_bound(&Value::String(year.to_string()), round_up, None, nanos);
        }
    }
    let text = v.as_str().unwrap_or_default();
    if round_up && (text.contains("||") || text.starts_with("now")) {
        if let Some((dt, Some(unit))) = parse_date_math(text) {
            let end = advance_unit(dt, unit)? - boostcore::time::Duration::milliseconds(1);
            let per: i128 = if nanos { 1 } else { 1_000_000 };
            return i64::try_from(end.unix_timestamp_nanos() / per).ok();
        }
    }
    date_number(v, format, nanos)
}

/// A date as the number the index holds: milliseconds, or nanoseconds for a
/// `date_nanos`.
///
/// This is `DateFieldMapper.Resolution` -- what OpenSearch stores, and what a
/// sort on a date reports. It is also the only representation with the range
/// dates need: text compares by spelling, so a year past 9999 stops ordering
/// correctly, and a count of nanoseconds in an i64 runs out in 2262.
pub fn date_number(v: &Value, format: Option<&str>, nanos: bool) -> Option<i64> {
    // a number is a count already, in whatever unit the format names
    let count = |n: f64| -> Option<i64> {
        let millis = match format {
            Some(f) if f.contains("epoch_second") => n * 1_000.0,
            _ => n,
        };
        let out = if nanos { millis * 1_000_000.0 } else { millis };
        (out.is_finite() && out.abs() < 9.2e18).then_some(out as i64)
    };
    let unit: i128 = if nanos { 1 } else { 1_000_000 };
    let read = |s: &str| -> Option<i64> {
        let dt = parse_date_lenient(s)?;
        i64::try_from(dt.unix_timestamp_nanos() / unit).ok()
    };
    match v {
        Value::Number(n) => count(n.as_f64()?),
        Value::String(s) => match s.parse::<f64>() {
            // a number written as text still means what the format says
            Ok(n) if format.is_some() => count(n),
            // `2019` is a year before it is a count of milliseconds, so the
            // date reading is tried first and the epoch only where nothing
            // else could be read from the digits
            Ok(n) => read(s).or_else(|| count(n)),
            _ => read(s),
        },
        _ => None,
    }
}

/// As `canonical_date_with`, but able to keep the whole fraction.
///
/// A `date` reports milliseconds and a `date_nanos` reports nanoseconds; the
/// finer resolution is the only reason the second type exists, so truncating
/// on the way in would throw away what it was chosen for.
pub fn canonical_date_prec(v: &Value, format: Option<&str>, nanos: bool) -> Option<String> {
    let scale: i128 = match format {
        Some(f) if f.contains("epoch_second") => 1_000_000_000,
        _ => 1_000_000,
    };
    let dt = match v {
        Value::Number(n) => boostcore::time::OffsetDateTime::from_unix_timestamp_nanos(
            (n.as_f64()? as i128) * scale,
        )
        .ok()?,
        Value::String(s) => match s.parse::<f64>() {
            // a number written as text still means what the format says
            Ok(n) if format.is_some() => {
                boostcore::time::OffsetDateTime::from_unix_timestamp_nanos((n as i128) * scale)
                    .ok()?
            }
            // `2019` is a year before it is a count of milliseconds, so the
            // date reading is tried first and the epoch only where nothing
            // else could be read from the digits
            Ok(n) => parse_date_lenient(s).or_else(|| {
                boostcore::time::OffsetDateTime::from_unix_timestamp_nanos((n as i128) * scale).ok()
            })?,
            _ => parse_date_lenient(s)?,
        },
        _ => return None,
    };
    if nanos {
        return Some(format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}Z",
            dt.year(),
            dt.month() as u8,
            dt.day(),
            dt.hour(),
            dt.minute(),
            dt.second(),
            dt.nanosecond(),
        ));
    }
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.millisecond(),
    ))
}

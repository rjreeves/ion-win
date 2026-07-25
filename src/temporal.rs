//! Native temporal values and operations. Values use canonical ISO text so
//! they remain ordinary shell words, while this module supplies validation,
//! comparison-safe normalization, arithmetic, extraction, and truncation.

use chrono::{
    DateTime, Datelike, Duration, FixedOffset, Local, LocalResult, NaiveDate, NaiveDateTime,
    NaiveTime, SecondsFormat, TimeZone, Timelike,
};
use chrono_tz::{GapInfo, Tz};

enum Timestamp {
    Offset(DateTime<FixedOffset>),
    Naive(NaiveDateTime),
}

pub fn date(value: &str) -> Result<String, String> {
    for fmt in ["%Y-%m-%d", "%Y-%-m-%-d"] {
        if let Ok(value) = NaiveDate::parse_from_str(value, fmt) {
            return Ok(value.format("%Y-%m-%d").to_string());
        }
    }
    Err(format!("expected date (YYYY-MM-DD), found value '{value}'"))
}

pub fn time(value: &str) -> Result<String, String> {
    for fmt in ["%H:%M:%S%.f", "%H:%M"] {
        if let Ok(value) = NaiveTime::parse_from_str(value, fmt) {
            return Ok(format_time(value));
        }
    }
    Err(format!("expected time (HH:MM[:SS]), found value '{value}'"))
}

pub fn datetime(value: &str) -> Result<String, String> {
    parse_datetime(value).map(|value| match value {
        Timestamp::Offset(value) => value.to_rfc3339_opts(SecondsFormat::AutoSi, false),
        Timestamp::Naive(value) => value.format("%Y-%m-%dT%H:%M:%S%.f").to_string(),
    })
}

pub fn duration(value: &str) -> Result<String, String> {
    parse_duration(value).map(format_duration)
}

pub fn now() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Secs, false)
}

pub fn today() -> String {
    Local::now().date_naive().format("%Y-%m-%d").to_string()
}

#[derive(Clone, Copy)]
enum AmbiguousPolicy {
    Reject,
    Earlier,
    Later,
}

#[derive(Clone, Copy)]
enum GapPolicy {
    Reject,
    ShiftForward,
    ShiftBackward,
}

/// Converts an offset datetime's instant into `zone`, or interprets a naive
/// datetime as a wall clock in `zone`. DST folds and gaps reject by default;
/// callers must explicitly choose how exceptional local times are resolved.
pub fn timezone(
    value: &str,
    zone: &str,
    ambiguous_policy: Option<&str>,
    gap_policy: Option<&str>,
) -> Result<String, String> {
    let zone: Tz = zone
        .parse()
        .map_err(|_| format!("timezone: unknown IANA timezone '{zone}'"))?;
    let ambiguous_policy = parse_ambiguous_policy(ambiguous_policy)?;
    let gap_policy = parse_gap_policy(gap_policy)?;

    let resolved = match parse_datetime(value)? {
        Timestamp::Offset(value) => value.with_timezone(&zone),
        Timestamp::Naive(value) => {
            resolve_local(value, zone, ambiguous_policy, gap_policy)?
        }
    };
    Ok(resolved.to_rfc3339_opts(SecondsFormat::AutoSi, false))
}

fn parse_ambiguous_policy(value: Option<&str>) -> Result<AmbiguousPolicy, String> {
    match value.unwrap_or("reject").to_ascii_lowercase().as_str() {
        "reject" => Ok(AmbiguousPolicy::Reject),
        "earlier" => Ok(AmbiguousPolicy::Earlier),
        "later" => Ok(AmbiguousPolicy::Later),
        value => Err(format!(
            "timezone: invalid ambiguous-time policy '{value}' (expected reject, earlier, or later)"
        )),
    }
}

fn parse_gap_policy(value: Option<&str>) -> Result<GapPolicy, String> {
    match value.unwrap_or("reject").to_ascii_lowercase().as_str() {
        "reject" => Ok(GapPolicy::Reject),
        "shift-forward" | "forward" => Ok(GapPolicy::ShiftForward),
        "shift-backward" | "backward" => Ok(GapPolicy::ShiftBackward),
        value => Err(format!(
            "timezone: invalid nonexistent-time policy '{value}' (expected reject, shift-forward, or shift-backward)"
        )),
    }
}

fn resolve_local(
    value: NaiveDateTime,
    zone: Tz,
    ambiguous_policy: AmbiguousPolicy,
    gap_policy: GapPolicy,
) -> Result<DateTime<Tz>, String> {
    match zone.from_local_datetime(&value) {
        LocalResult::Single(value) => Ok(value),
        LocalResult::Ambiguous(earlier, later) => match ambiguous_policy {
            AmbiguousPolicy::Earlier => Ok(earlier),
            AmbiguousPolicy::Later => Ok(later),
            AmbiguousPolicy::Reject => Err(format!(
                "timezone: local time '{value}' is ambiguous in {zone}; choose earlier or later"
            )),
        },
        LocalResult::None => match gap_policy {
            GapPolicy::Reject => Err(format!(
                "timezone: local time '{value}' does not exist in {zone}; choose shift-forward or shift-backward"
            )),
            policy => resolve_gap(value, zone, policy),
        },
    }
}

fn resolve_gap(
    value: NaiveDateTime,
    zone: Tz,
    policy: GapPolicy,
) -> Result<DateTime<Tz>, String> {
    let gap = GapInfo::new(&value, &zone)
        .ok_or_else(|| format!("timezone: cannot resolve nonexistent local time '{value}'"))?;
    let (Some((begin, _)), Some(end)) = (gap.begin, gap.end) else {
        return Err(format!(
            "timezone: transition data cannot resolve local time '{value}' in {zone}"
        ));
    };
    let width = end.naive_local().signed_duration_since(begin);
    let shifted = match policy {
        GapPolicy::ShiftForward => value + width,
        GapPolicy::ShiftBackward => value - width,
        GapPolicy::Reject => unreachable!(),
    };
    zone.from_local_datetime(&shifted).single().ok_or_else(|| {
        format!("timezone: shifted local time '{shifted}' is still not unique in {zone}")
    })
}

pub fn extract(part: &str, value: &str) -> Result<String, String> {
    let part = part.to_ascii_lowercase();
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return extract_parts(&part, date.year(), date.month(), date.day(), 0, 0, 0, 0);
    }
    if let Ok(time) = NaiveTime::parse_from_str(value, "%H:%M:%S%.f") {
        return extract_parts(
            &part,
            0,
            0,
            0,
            time.hour(),
            time.minute(),
            time.second(),
            time.nanosecond(),
        );
    }
    match parse_datetime(value)? {
        Timestamp::Offset(value) => extract_parts(
            &part,
            value.year(),
            value.month(),
            value.day(),
            value.hour(),
            value.minute(),
            value.second(),
            value.nanosecond(),
        ),
        Timestamp::Naive(value) => extract_parts(
            &part,
            value.year(),
            value.month(),
            value.day(),
            value.hour(),
            value.minute(),
            value.second(),
            value.nanosecond(),
        ),
    }
}

fn extract_parts(
    part: &str,
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    nanos: u32,
) -> Result<String, String> {
    let value = match part {
        "year" => year.to_string(),
        "month" => month.to_string(),
        "day" => day.to_string(),
        "hour" => hour.to_string(),
        "minute" => minute.to_string(),
        "second" if nanos == 0 => second.to_string(),
        "second" => format!("{:.9}", second as f64 + nanos as f64 / 1_000_000_000.0)
            .trim_end_matches('0')
            .to_string(),
        "epoch" => return Err("extract: epoch requires a datetime with an explicit offset".into()),
        _ => return Err(format!("extract: unsupported field '{part}'")),
    };
    Ok(value)
}

pub fn truncate(part: &str, value: &str) -> Result<String, String> {
    let part = part.to_ascii_lowercase();
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        let date = truncate_date(part.as_str(), date)?;
        return Ok(date.format("%Y-%m-%d").to_string());
    }
    match parse_datetime(value)? {
        Timestamp::Naive(value) => Ok(truncate_naive(part.as_str(), value)?
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string()),
        Timestamp::Offset(value) => {
            let offset = *value.offset();
            let naive = truncate_naive(part.as_str(), value.naive_local())?;
            Ok(DateTime::<FixedOffset>::from_naive_utc_and_offset(
                naive - Duration::seconds(i64::from(offset.local_minus_utc())),
                offset,
            )
            .to_rfc3339_opts(SecondsFormat::Secs, false))
        }
    }
}

pub fn add(value: &str, interval: &str) -> Result<String, String> {
    shift(value, parse_duration(interval)?)
}

pub fn subtract(value: &str, interval: &str) -> Result<String, String> {
    shift(value, -parse_duration(interval)?)
}

fn shift(value: &str, amount: Duration) -> Result<String, String> {
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        if amount.num_milliseconds() % 86_400_000 != 0 {
            return Err("date arithmetic requires a whole-day duration".into());
        }
        let shifted = date
            .checked_add_signed(amount)
            .ok_or_else(|| "date arithmetic overflow".to_string())?;
        return Ok(shifted.format("%Y-%m-%d").to_string());
    }
    match parse_datetime(value)? {
        Timestamp::Naive(value) => value
            .checked_add_signed(amount)
            .map(|v| v.format("%Y-%m-%dT%H:%M:%S%.f").to_string())
            .ok_or_else(|| "datetime arithmetic overflow".into()),
        Timestamp::Offset(value) => value
            .checked_add_signed(amount)
            .map(|v| v.to_rfc3339_opts(SecondsFormat::AutoSi, false))
            .ok_or_else(|| "datetime arithmetic overflow".into()),
    }
}

pub fn format(value: &str, pattern: &str) -> Result<String, String> {
    let pattern = match pattern {
        "dd-MMM-yy" => "%d-%b-%y",
        pattern => pattern,
    };
    if let Ok(value) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(value.format(pattern).to_string());
    }
    if let Ok(value) = NaiveTime::parse_from_str(value, "%H:%M:%S%.f") {
        return Ok(value.format(pattern).to_string());
    }
    Ok(match parse_datetime(value)? {
        Timestamp::Naive(value) => value.format(pattern).to_string(),
        Timestamp::Offset(value) => value.format(pattern).to_string(),
    })
}

pub fn compare(left: &str, right: &str) -> Result<String, String> {
    use std::cmp::Ordering;
    let ordering = match (parse_comparable(left)?, parse_comparable(right)?) {
        (Comparable::Date(a), Comparable::Date(b)) => a.cmp(&b),
        (Comparable::Time(a), Comparable::Time(b)) => a.cmp(&b),
        (Comparable::Naive(a), Comparable::Naive(b)) => a.cmp(&b),
        (Comparable::Instant(a), Comparable::Instant(b)) => a.cmp(&b),
        _ => return Err("date_compare: values must have matching temporal kinds".into()),
    };
    Ok(match ordering {
        Ordering::Less => "-1",
        Ordering::Equal => "0",
        Ordering::Greater => "1",
    }
    .into())
}

pub fn difference(left: &str, right: &str) -> Result<String, String> {
    let delta = match (parse_comparable(left)?, parse_comparable(right)?) {
        (Comparable::Date(a), Comparable::Date(b)) => a.signed_duration_since(b),
        (Comparable::Time(a), Comparable::Time(b)) => a.signed_duration_since(b),
        (Comparable::Naive(a), Comparable::Naive(b)) => a.signed_duration_since(b),
        (Comparable::Instant(a), Comparable::Instant(b)) => a.signed_duration_since(b),
        _ => return Err("date_diff: values must have matching temporal kinds".into()),
    };
    Ok(format_duration(delta))
}

enum Comparable {
    Date(NaiveDate),
    Time(NaiveTime),
    Naive(NaiveDateTime),
    Instant(DateTime<FixedOffset>),
}

fn parse_comparable(value: &str) -> Result<Comparable, String> {
    if let Ok(value) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(Comparable::Date(value));
    }
    if let Ok(value) = NaiveTime::parse_from_str(value, "%H:%M:%S%.f") {
        return Ok(Comparable::Time(value));
    }
    Ok(match parse_datetime(value)? {
        Timestamp::Naive(value) => Comparable::Naive(value),
        Timestamp::Offset(value) => Comparable::Instant(value),
    })
}

fn parse_datetime(value: &str) -> Result<Timestamp, String> {
    if let Ok(value) = DateTime::parse_from_rfc3339(value) {
        return Ok(Timestamp::Offset(value));
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(value) = NaiveDateTime::parse_from_str(value, fmt) {
            return Ok(Timestamp::Naive(value));
        }
    }
    Err(format!(
        "expected datetime (ISO 8601), found value '{value}'"
    ))
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    if let Some(seconds) = value.strip_prefix("PT").and_then(|v| v.strip_suffix('S')) {
        return seconds
            .parse::<f64>()
            .map(|seconds| Duration::milliseconds((seconds * 1000.0).round() as i64))
            .map_err(|_| format!("expected duration, found value '{value}'"));
    }
    let words: Vec<&str> = value.split_whitespace().collect();
    if words.is_empty() || words.len() % 2 != 0 {
        return Err(format!(
            "expected duration such as '2 days 3 hours', found value '{value}'"
        ));
    }
    let mut total = Duration::zero();
    for pair in words.chunks_exact(2) {
        let number = pair[0]
            .parse::<i64>()
            .map_err(|_| format!("duration: invalid number '{}'", pair[0]))?;
        let unit = pair[1].trim_end_matches('s').to_ascii_lowercase();
        let amount = match unit.as_str() {
            "week" => Duration::weeks(number),
            "day" => Duration::days(number),
            "hour" => Duration::hours(number),
            "minute" => Duration::minutes(number),
            "second" => Duration::seconds(number),
            "millisecond" => Duration::milliseconds(number),
            _ => return Err(format!("duration: unsupported unit '{}'", pair[1])),
        };
        total = total
            .checked_add(&amount)
            .ok_or_else(|| "duration overflow".to_string())?;
    }
    Ok(total)
}

fn format_duration(value: Duration) -> String {
    let millis = value.num_milliseconds();
    if millis % 1000 == 0 {
        return format!("PT{}S", millis / 1000);
    }
    let sign = if millis < 0 { "-" } else { "" };
    let absolute = millis.unsigned_abs();
    let fraction = format!("{:03}", absolute % 1000)
        .trim_end_matches('0')
        .to_string();
    format!("PT{sign}{}.{fraction}S", absolute / 1000)
}

fn format_time(value: NaiveTime) -> String {
    value.format("%H:%M:%S%.f").to_string()
}

fn truncate_date(part: &str, value: NaiveDate) -> Result<NaiveDate, String> {
    match part {
        "year" => NaiveDate::from_ymd_opt(value.year(), 1, 1),
        "month" => NaiveDate::from_ymd_opt(value.year(), value.month(), 1),
        "day" => Some(value),
        _ => return Err(format!("date_trunc: unsupported precision '{part}'")),
    }
    .ok_or_else(|| "date_trunc: result is out of range".into())
}

fn truncate_naive(part: &str, value: NaiveDateTime) -> Result<NaiveDateTime, String> {
    let date = truncate_date(
        if matches!(part, "hour" | "minute" | "second") {
            "day"
        } else {
            part
        },
        value.date(),
    )?;
    let time = match part {
        "year" | "month" | "day" => NaiveTime::from_hms_opt(0, 0, 0),
        "hour" => NaiveTime::from_hms_opt(value.hour(), 0, 0),
        "minute" => NaiveTime::from_hms_opt(value.hour(), value.minute(), 0),
        "second" => NaiveTime::from_hms_opt(value.hour(), value.minute(), value.second()),
        _ => return Err(format!("date_trunc: unsupported precision '{part}'")),
    }
    .ok_or_else(|| "date_trunc: result is out of range".to_string())?;
    Ok(date.and_time(time))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_constructors_reject_impossible_values() {
        assert_eq!(date("2026-7-3").unwrap(), "2026-07-03");
        assert_eq!(time("9:05").unwrap(), "09:05:00");
        assert!(date("2026-02-30").is_err());
    }

    #[test]
    fn intervals_add_and_subtract() {
        assert_eq!(duration("1 day 2 hours").unwrap(), "PT93600S");
        assert_eq!(add("2026-07-03", "2 days").unwrap(), "2026-07-05");
        assert_eq!(
            subtract("2026-07-03T12:00:00+10:00", "3 hours").unwrap(),
            "2026-07-03T09:00:00+10:00"
        );
        assert_eq!(duration("1500 milliseconds").unwrap(), "PT1.5S");
        assert!(add("2026-07-03", "2 hours").is_err());
    }

    #[test]
    fn extracts_and_truncates() {
        assert_eq!(extract("month", "2026-07-03").unwrap(), "7");
        assert_eq!(
            truncate("month", "2026-07-23T12:34:56").unwrap(),
            "2026-07-01T00:00:00"
        );
    }

    #[test]
    fn compares_offset_datetimes_by_instant_and_computes_difference() {
        assert_eq!(
            compare("2026-07-03T10:00:00+10:00", "2026-07-03T00:00:00Z").unwrap(),
            "0"
        );
        assert_eq!(difference("2026-07-05", "2026-07-03").unwrap(), "PT172800S");
    }

    #[test]
    fn friendly_day_month_year_format_is_supported() {
        assert_eq!(format("2026-07-23", "dd-MMM-yy").unwrap(), "23-Jul-26");
    }

    #[test]
    fn named_timezone_conversion_uses_seasonal_offsets() {
        assert_eq!(
            timezone("2026-01-15T00:00:00Z", "Australia/Sydney", None, None).unwrap(),
            "2026-01-15T11:00:00+11:00"
        );
        assert_eq!(
            timezone("2026-07-15T00:00:00Z", "Australia/Sydney", None, None).unwrap(),
            "2026-07-15T10:00:00+10:00"
        );
    }

    #[test]
    fn ambiguous_local_time_requires_an_explicit_choice() {
        let value = "2026-04-05T02:30:00";
        assert!(timezone(value, "Australia/Sydney", None, None)
            .unwrap_err()
            .contains("ambiguous"));
        assert_eq!(
            timezone(value, "Australia/Sydney", Some("earlier"), None).unwrap(),
            "2026-04-05T02:30:00+11:00"
        );
        assert_eq!(
            timezone(value, "Australia/Sydney", Some("later"), None).unwrap(),
            "2026-04-05T02:30:00+10:00"
        );
    }

    #[test]
    fn nonexistent_local_time_can_shift_across_the_dst_gap() {
        let value = "2026-10-04T02:30:00";
        assert!(timezone(value, "Australia/Sydney", None, None)
            .unwrap_err()
            .contains("does not exist"));
        assert_eq!(
            timezone(
                value,
                "Australia/Sydney",
                None,
                Some("shift-forward")
            )
            .unwrap(),
            "2026-10-04T03:30:00+11:00"
        );
        assert_eq!(
            timezone(
                value,
                "Australia/Sydney",
                None,
                Some("shift-backward")
            )
            .unwrap(),
            "2026-10-04T01:30:00+10:00"
        );
    }

    #[test]
    fn unknown_zones_and_policies_are_rejected() {
        assert!(timezone("2026-01-01T00:00:00", "Mars/Olympus", None, None).is_err());
        assert!(timezone(
            "2026-01-01T00:00:00",
            "Australia/Sydney",
            Some("guess"),
            None
        )
        .is_err());
    }
}

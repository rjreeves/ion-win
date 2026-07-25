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
    parse_interval(value).map(format_interval)
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
    shift(value, parse_interval(interval)?)
}

pub fn subtract(value: &str, interval: &str) -> Result<String, String> {
    shift(value, parse_interval(interval)?.checked_neg()?)
}

pub fn add_in_timezone(
    value: &str,
    interval: &str,
    zone: &str,
    ambiguous_policy: Option<&str>,
    gap_policy: Option<&str>,
) -> Result<String, String> {
    shift_in_timezone(
        value,
        parse_interval(interval)?,
        zone,
        ambiguous_policy,
        gap_policy,
    )
}

pub fn subtract_in_timezone(
    value: &str,
    interval: &str,
    zone: &str,
    ambiguous_policy: Option<&str>,
    gap_policy: Option<&str>,
) -> Result<String, String> {
    shift_in_timezone(
        value,
        parse_interval(interval)?.checked_neg()?,
        zone,
        ambiguous_policy,
        gap_policy,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Interval {
    months: i64,
    fixed: Duration,
}

impl Interval {
    fn zero() -> Self {
        Self {
            months: 0,
            fixed: Duration::zero(),
        }
    }

    fn checked_neg(self) -> Result<Self, String> {
        Ok(Self {
            months: self
                .months
                .checked_neg()
                .ok_or_else(|| "interval overflow".to_string())?,
            fixed: self
                .fixed
                .checked_mul(-1)
                .ok_or_else(|| "interval overflow".to_string())?,
        })
    }
}

fn shift(value: &str, amount: Interval) -> Result<String, String> {
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        if amount.fixed.num_milliseconds() % 86_400_000 != 0 {
            return Err("date arithmetic requires a whole-day duration".into());
        }
        let shifted = shift_months(date, amount.months)?
            .checked_add_signed(amount.fixed)
            .ok_or_else(|| "date arithmetic overflow".to_string())?;
        return Ok(shifted.format("%Y-%m-%d").to_string());
    }
    match parse_datetime(value)? {
        Timestamp::Naive(value) => shift_naive(value, amount)
            .map(|v| v.format("%Y-%m-%dT%H:%M:%S%.f").to_string()),
        Timestamp::Offset(value) => {
            let offset = *value.offset();
            let local = shift_naive(value.naive_local(), amount)?;
            offset
                .from_local_datetime(&local)
                .single()
                .map(|v| v.to_rfc3339_opts(SecondsFormat::AutoSi, false))
                .ok_or_else(|| "datetime arithmetic overflow".into())
        }
    }
}

fn shift_naive(value: NaiveDateTime, amount: Interval) -> Result<NaiveDateTime, String> {
    let calendar_shifted = shift_months(value.date(), amount.months)?.and_time(value.time());
    calendar_shifted
        .checked_add_signed(amount.fixed)
        .ok_or_else(|| "datetime arithmetic overflow".into())
}

fn shift_in_timezone(
    value: &str,
    amount: Interval,
    zone: &str,
    ambiguous_policy: Option<&str>,
    gap_policy: Option<&str>,
) -> Result<String, String> {
    let zone: Tz = zone
        .parse()
        .map_err(|_| format!("date arithmetic: unknown IANA timezone '{zone}'"))?;
    let ambiguous_policy = parse_ambiguous_policy(ambiguous_policy)?;
    let gap_policy = parse_gap_policy(gap_policy)?;
    let start = match parse_datetime(value)? {
        Timestamp::Offset(value) => value.with_timezone(&zone),
        Timestamp::Naive(value) => resolve_local(value, zone, ambiguous_policy, gap_policy)?,
    };
    let calendar_local =
        shift_months(start.date_naive(), amount.months)?.and_time(start.time());
    let calendar_resolved =
        resolve_local(calendar_local, zone, ambiguous_policy, gap_policy)?;
    calendar_resolved
        .checked_add_signed(amount.fixed)
        .map(|value| value.to_rfc3339_opts(SecondsFormat::AutoSi, false))
        .ok_or_else(|| "datetime arithmetic overflow".into())
}

fn shift_months(value: NaiveDate, months: i64) -> Result<NaiveDate, String> {
    let month_index = i64::from(value.year())
        .checked_mul(12)
        .and_then(|v| v.checked_add(i64::from(value.month0())))
        .and_then(|v| v.checked_add(months))
        .ok_or_else(|| "calendar interval overflow".to_string())?;
    let year = month_index.div_euclid(12);
    let month = month_index.rem_euclid(12) as u32 + 1;
    let year = i32::try_from(year).map_err(|_| "calendar interval overflow".to_string())?;
    let last_day = last_day_of_month(year, month)?;
    NaiveDate::from_ymd_opt(year, month, value.day().min(last_day))
        .ok_or_else(|| "calendar interval overflow".to_string())
}

fn last_day_of_month(year: i32, month: u32) -> Result<u32, String> {
    let (next_year, next_month) = if month == 12 {
        (year.checked_add(1).ok_or_else(|| "calendar interval overflow".to_string())?, 1)
    } else {
        (year, month + 1)
    };
    let next = NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .ok_or_else(|| "calendar interval overflow".to_string())?;
    Ok(next
        .pred_opt()
        .ok_or_else(|| "calendar interval overflow".to_string())?
        .day())
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

fn parse_interval(value: &str) -> Result<Interval, String> {
    let value = value.trim();
    if value.starts_with('P') && !value.starts_with("PT") {
        return parse_calendar_interval(value);
    }
    if let Some(seconds) = value.strip_prefix("PT").and_then(|v| v.strip_suffix('S')) {
        return seconds
            .parse::<f64>()
            .map(|seconds| Interval {
                months: 0,
                fixed: Duration::milliseconds((seconds * 1000.0).round() as i64),
            })
            .map_err(|_| format!("expected duration, found value '{value}'"));
    }
    let words: Vec<&str> = value.split_whitespace().collect();
    if words.is_empty() || words.len() % 2 != 0 {
        return Err(format!(
            "expected duration such as '2 days 3 hours', found value '{value}'"
        ));
    }
    let mut total = Interval::zero();
    for pair in words.chunks_exact(2) {
        let number = pair[0]
            .parse::<i64>()
            .map_err(|_| format!("duration: invalid number '{}'", pair[0]))?;
        let unit = pair[1].trim_end_matches('s').to_ascii_lowercase();
        let amount = match unit.as_str() {
            "year" => {
                total.months = total
                    .months
                    .checked_add(number.checked_mul(12).ok_or_else(|| "interval overflow".to_string())?)
                    .ok_or_else(|| "interval overflow".to_string())?;
                continue;
            }
            "month" => {
                total.months = total
                    .months
                    .checked_add(number)
                    .ok_or_else(|| "interval overflow".to_string())?;
                continue;
            }
            "week" => Duration::weeks(number),
            "day" => Duration::days(number),
            "hour" => Duration::hours(number),
            "minute" => Duration::minutes(number),
            "second" => Duration::seconds(number),
            "millisecond" => Duration::milliseconds(number),
            _ => return Err(format!("duration: unsupported unit '{}'", pair[1])),
        };
        total.fixed = total
            .fixed
            .checked_add(&amount)
            .ok_or_else(|| "interval overflow".to_string())?;
    }
    Ok(total)
}

fn parse_calendar_interval(value: &str) -> Result<Interval, String> {
    let (calendar, fixed) = value.split_once(';').unwrap_or((value, ""));
    let mut body = calendar
        .strip_prefix('P')
        .ok_or_else(|| format!("expected interval, found value '{value}'"))?;
    let mut months = 0i64;
    if let Some(year_end) = body.find('Y') {
        let years = body[..year_end]
            .parse::<i64>()
            .map_err(|_| format!("expected interval, found value '{value}'"))?;
        months = years
            .checked_mul(12)
            .ok_or_else(|| "interval overflow".to_string())?;
        body = &body[year_end + 1..];
    }
    if let Some(months_text) = body.strip_suffix('M') {
        months = months
            .checked_add(
                months_text
                    .parse::<i64>()
                    .map_err(|_| format!("expected interval, found value '{value}'"))?,
            )
            .ok_or_else(|| "interval overflow".to_string())?;
        body = "";
    }
    if !body.is_empty() || months == 0 {
        return Err(format!("expected interval, found value '{value}'"));
    }
    let fixed = if fixed.is_empty() {
        Duration::zero()
    } else {
        let parsed = parse_interval(fixed)?;
        if parsed.months != 0 {
            return Err(format!("expected fixed duration after ';', found '{fixed}'"));
        }
        parsed.fixed
    };
    Ok(Interval { months, fixed })
}

fn format_interval(value: Interval) -> String {
    if value.months == 0 {
        return format_duration(value.fixed);
    }
    let sign = if value.months < 0 { "-" } else { "" };
    let absolute = value.months.unsigned_abs();
    let years = absolute / 12;
    let months = absolute % 12;
    let mut output = "P".to_string();
    if years != 0 {
        output.push_str(&format!("{sign}{years}Y"));
    }
    if months != 0 {
        output.push_str(&format!("{sign}{months}M"));
    }
    if value.fixed != Duration::zero() {
        output.push(';');
        output.push_str(&format_duration(value.fixed));
    }
    output
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
    fn calendar_intervals_clamp_month_ends_and_round_trip() {
        assert_eq!(duration("1 year 2 months").unwrap(), "P1Y2M");
        assert_eq!(duration("P1Y2M").unwrap(), "P1Y2M");
        assert_eq!(duration("-1 year -2 months").unwrap(), "P-1Y-2M");
        assert_eq!(duration("1 month 2 days").unwrap(), "P1M;PT172800S");
        assert_eq!(duration("P1M;PT172800S").unwrap(), "P1M;PT172800S");

        assert_eq!(add("2025-01-31", "1 month").unwrap(), "2025-02-28");
        assert_eq!(add("2024-01-31", "1 month").unwrap(), "2024-02-29");
        assert_eq!(subtract("2024-03-31", "1 month").unwrap(), "2024-02-29");
        assert_eq!(add("2024-02-29", "1 year").unwrap(), "2025-02-28");
        assert_eq!(
            add("2026-01-31T10:15:00+10:00", "1 month 2 hours").unwrap(),
            "2026-02-28T12:15:00+10:00"
        );
    }

    #[test]
    fn named_zone_calendar_arithmetic_preserves_wall_clock_across_dst() {
        assert_eq!(
            add_in_timezone(
                "2026-09-04T09:00:00",
                "1 month",
                "Australia/Sydney",
                None,
                None,
            )
            .unwrap(),
            "2026-10-04T09:00:00+11:00"
        );

        let gap = add_in_timezone(
            "2026-09-04T02:30:00",
            "1 month",
            "Australia/Sydney",
            None,
            None,
        )
        .unwrap_err();
        assert!(gap.contains("does not exist"));
        assert_eq!(
            add_in_timezone(
                "2026-09-04T02:30:00",
                "1 month",
                "Australia/Sydney",
                None,
                Some("shift-forward"),
            )
            .unwrap(),
            "2026-10-04T03:30:00+11:00"
        );
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

//! Date literals: parsing the values users type (fixed or relative like
//! `28d`), and rendering them in the warehouse's own literal form.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use regex::Regex;
use std::sync::LazyLock;
use zsql_dialect::Dialect;
use zsql_semantic::Field;

fn dynamic_re() -> &'static Regex {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(\d{1,3})([hdwmqy])").unwrap());
    &RE
}

pub fn is_dynamic(value: &str) -> bool {
    dynamic_re().is_match(value)
}

/// Rails `ParseableDate#dynamic_date_to_date`: `n` units ago, snapped to the
/// beginning or end of that unit unless the unit is hours or days.
pub fn dynamic_to_date(value: &str, is_ending: bool) -> Option<NaiveDateTime> {
    let lowered = value.trim().to_lowercase();
    let caps = dynamic_re().captures(&lowered)?;
    let n: i64 = caps[1].parse().ok()?;
    let unit = caps[2].to_string();
    let unit = unit.as_str();
    let now = chrono::Utc::now().naive_utc();
    let dt = match unit {
        "h" => now - Duration::hours(n),
        "d" => now - Duration::days(n),
        "w" => now - Duration::weeks(n),
        "m" => shift_months(now, -(n as i32)),
        "q" => now - Duration::days(n * 90),
        "y" => shift_months(now, -(n as i32) * 12),
        _ => return None,
    };
    Some(match unit {
        "h" | "d" => dt,
        "w" => {
            // beginning/end of week, Sunday start
            let days_from_sunday = dt.weekday().num_days_from_sunday() as i64;
            let start = dt.date() - Duration::days(days_from_sunday);
            if is_ending {
                (start + Duration::days(6)).and_time(NaiveTime::from_hms_opt(23, 59, 59).unwrap())
            } else {
                start.and_time(NaiveTime::MIN)
            }
        }
        "m" => {
            let first = NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1).unwrap();
            if is_ending {
                (shift_months(first.and_time(NaiveTime::MIN), 1).date() - Duration::days(1))
                    .and_time(NaiveTime::from_hms_opt(23, 59, 59).unwrap())
            } else {
                first.and_time(NaiveTime::MIN)
            }
        }
        "q" => {
            let qm = (dt.month() - 1) / 3 * 3 + 1;
            let first = NaiveDate::from_ymd_opt(dt.year(), qm, 1).unwrap();
            if is_ending {
                (shift_months(first.and_time(NaiveTime::MIN), 3).date() - Duration::days(1))
                    .and_time(NaiveTime::from_hms_opt(23, 59, 59).unwrap())
            } else {
                first.and_time(NaiveTime::MIN)
            }
        }
        "y" => {
            if is_ending {
                NaiveDate::from_ymd_opt(dt.year(), 12, 31).unwrap().and_time(NaiveTime::from_hms_opt(23, 59, 59).unwrap())
            } else {
                NaiveDate::from_ymd_opt(dt.year(), 1, 1).unwrap().and_time(NaiveTime::MIN)
            }
        }
        _ => dt,
    })
}

fn shift_months(dt: NaiveDateTime, months: i32) -> NaiveDateTime {
    let total = dt.year() * 12 + dt.month() as i32 - 1 + months;
    let (y, m) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
    let last = NaiveDate::from_ymd_opt(y, m + 1, 1).unwrap_or_else(|| NaiveDate::from_ymd_opt(y + 1, 1, 1).unwrap()) - Duration::days(1);
    let d = dt.day().min(last.day());
    NaiveDate::from_ymd_opt(y, m, d).unwrap().and_time(dt.time())
}

/// A fixed or dynamic value as a timestamp, or None when unparseable.
pub fn parse_value(value: &str, is_ending: bool) -> Option<NaiveDateTime> {
    if is_dynamic(value) {
        return dynamic_to_date(value, is_ending);
    }
    parse_fixed(value)
}

/// The subset of `DateTime.parse` the goldens use.
pub fn parse_fixed(value: &str) -> Option<NaiveDateTime> {
    let v = value.trim();
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M", "%Y/%m/%d %H:%M:%S"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(v, fmt) {
            return Some(dt);
        }
    }
    for fmt in ["%Y-%m-%d", "%Y/%m/%d", "%d-%m-%Y", "%b %d %Y", "%B %d %Y", "%d %b %Y"] {
        if let Ok(d) = NaiveDate::parse_from_str(v, fmt) {
            return Some(d.and_time(NaiveTime::MIN));
        }
    }
    None
}

/// Rails `Planner::Sql::Dates#date_lit`.
pub fn date_lit(dialect: &Dialect, field: &Field, value: &str, is_ending: bool) -> Result<String, String> {
    let mut text = value.to_string();
    if is_dynamic(value) {
        let dt = dynamic_to_date(value, is_ending).ok_or_else(|| format!("cannot parse dynamic date {value}"))?;
        let unit = dynamic_re().captures(&value.to_lowercase()).map(|c| c[2].to_string()).unwrap_or_default();
        text = if unit == "h" { dt.format(dialect.date_time_format()).to_string() } else { dt.format(dialect.date_format()).to_string() };
    }
    static TIME_OF_DAY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+\d\d:\d\d:\d\d").unwrap());
    if dialect.extend_ending_date_to_last_hour_of_day() && is_ending && !TIME_OF_DAY.is_match(&text) {
        text.push_str(" 23:59:59");
    }
    let dt = parse_fixed(&text).ok_or_else(|| format!("cannot parse date {text}"))?;
    let is_timestamp_field = matches!(field.data_type, Some(zsql_semantic::DataType::DateTime));
    let rendered = if is_timestamp_field && dt.hour() > 0 {
        dialect.timestamp_literal(&dt.format(dialect.date_time_format()).to_string())
    } else {
        dialect.date_literal(&dt.format(dialect.date_format()).to_string())
    };
    rendered.map_err(|e| e.to_string())
}

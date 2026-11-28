//! The clock, as the records write it.

use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

/// Now, in RFC 3339 and UTC, to the millisecond.
pub fn now() -> String {
    let n = nanos();
    format_millis((n / 1_000_000) as i64)
}

/// A count of milliseconds since 1970 as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub(crate) fn format_millis(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// How many whole seconds ago a time this module wrote was; none for a
/// time it could not read.
pub(crate) fn age_secs(at: &str) -> Option<u64> {
    let then = millis_of(at)?;
    let now = (nanos() / 1_000_000) as i64;
    Some(((now - then).max(0) / 1000) as u64)
}

/// A time as `format_millis` writes it, as milliseconds since 1970.
pub(crate) fn millis_of(at: &str) -> Option<i64> {
    let b = at.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| at.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let ms = if b.get(19) == Some(&b'.') { num(20..23).unwrap_or(0) } else { 0 };
    // Howard Hinnant's `days_from_civil`.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + hh * 3600 + mm * 60 + ss) * 1000) + ms)
}

/// The calendar day of a count of days since 1970-01-01 (Howard
/// Hinnant's `civil_from_days`).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    #[test]
    fn days_are_calendar_days() {
        assert_eq!(super::format_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(super::format_millis(1_709_164_800_123), "2024-02-29T00:00:00.123Z");
        assert_eq!(super::format_millis(1_798_761_599_000), "2026-12-31T23:59:59.000Z");
        for ms in [0, 1_709_164_800_123, 1_798_761_599_000] {
            assert_eq!(super::millis_of(&super::format_millis(ms)), Some(ms));
        }
        assert_eq!(super::millis_of("yesterday"), None);
    }
}

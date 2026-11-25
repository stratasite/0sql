/// Now, as `utc_minute` writes it.
pub fn now() -> String {
    utc_minute(std::time::SystemTime::now())
}

/// UTC, to the minute: `2026-10-01 15:04`.
pub fn utc_minute(t: std::time::SystemTime) -> String {
    let (y, m, d, hour, minute, _) = civil(t);
    format!("{y:04}-{m:02}-{d:02} {hour:02}:{minute:02}")
}

/// UTC, to the millisecond, as a name can hold it and a listing sorts
/// it: `20261001T150405.123Z`.
pub fn utc_stamp(t: std::time::SystemTime) -> String {
    let (y, m, d, hour, minute, second) = civil(t);
    let millis = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_millis()).unwrap_or(0);
    format!("{y:04}{m:02}{d:02}T{hour:02}{minute:02}{second:02}.{millis:03}Z")
}

/// The year, month, day, hour, minute and second of a time, in UTC.
fn civil(t: std::time::SystemTime) -> (i64, i64, i64, i64, i64, i64) {
    let secs = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    // Civil-from-days (Howard Hinnant), so no date crate is needed here.
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

//! How long a stored result answers: `cache_ttl`, on a datasource in
//! `datasources.yml` or on one request to run.
//!
//! Written as whole seconds (`1800`), or with a unit: `10s`, `30m`,
//! `1h`, `10d`. `0` keeps nothing, and `-1` never expires. A number or
//! its text are the same setting, so `1800` and `"1800"` agree.

use std::time::Duration;

/// How long a stored result answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTtl {
    /// For this long after it finished; zero keeps nothing.
    For(Duration),
    /// Until it is invalidated or run again fresh.
    Never,
}

impl CacheTtl {
    /// What a datasource that says nothing keeps a result for.
    pub const DEFAULT: CacheTtl = CacheTtl::For(Duration::from_secs(24 * 3600));

    /// Reads one as it is written; the error says what was wrong with it.
    pub fn parse(text: &str) -> Result<CacheTtl, String> {
        let t = text.trim();
        let wrong =
            || format!("cache_ttl {t:?} is not a duration: write seconds (1800), or 10s, 30m, 1h, 10d; 0 keeps nothing, -1 never expires");
        if t == "-1" {
            return Ok(CacheTtl::Never);
        }
        let (digits, unit) = match t.char_indices().last() {
            Some((at, c)) if c.is_ascii_alphabetic() => (&t[..at], &t[at..]),
            _ => (t, "s"),
        };
        let scale: u64 = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86_400,
            _ => return Err(wrong()),
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(wrong());
        }
        let n: u64 = digits.parse().map_err(|_| wrong())?;
        n.checked_mul(scale).map(|s| CacheTtl::For(Duration::from_secs(s))).ok_or_else(wrong)
    }

    /// From a JSON value: a whole number, or text as [`CacheTtl::parse`] reads it.
    pub fn from_json(value: &serde_json::Value) -> Result<CacheTtl, String> {
        match value {
            serde_json::Value::Number(n) if n.is_i64() || n.is_u64() => CacheTtl::parse(&n.to_string()),
            serde_json::Value::String(s) => CacheTtl::parse(s),
            other => Err(format!(
                "cache_ttl {other} is not a duration: write seconds (1800), or 10s, 30m, 1h, 10d; 0 keeps nothing, -1 never expires"
            )),
        }
    }

    /// From a YAML value, as `datasources.yml` and `.zsql` hold it.
    pub fn from_yaml(value: &serde_yaml::Value) -> Result<CacheTtl, String> {
        match value {
            serde_yaml::Value::Number(n) if n.is_i64() || n.is_u64() => CacheTtl::parse(&n.to_string()),
            serde_yaml::Value::String(s) => CacheTtl::parse(s),
            other => Err(format!(
                "cache_ttl {} is not a duration: write seconds (1800), or 10s, 30m, 1h, 10d; 0 keeps nothing, -1 never expires",
                serde_yaml::to_string(other).unwrap_or_default().trim()
            )),
        }
    }

    /// The TTL a run is kept with: the request's over the datasource's,
    /// a day when neither says. A datasource that keeps nothing keeps
    /// nothing whatever the request asks: one that asks to keep its
    /// result there is refused, not quietly run uncached.
    pub fn resolve(asked: Option<CacheTtl>, datasource_uid: &str, datasource: Option<CacheTtl>) -> Result<CacheTtl, String> {
        match (asked, datasource) {
            (Some(a), Some(d)) if a.keeps() && !d.keeps() => Err(format!(
                "datasource {datasource_uid} keeps no results (its cache_ttl is 0), so a request cannot ask to cache one (cache_ttl {a}); send cache_ttl 0 or none"
            )),
            (Some(a), _) => Ok(a),
            (None, d) => Ok(d.unwrap_or(CacheTtl::DEFAULT)),
        }
    }

    /// Whether a result is kept and found again at all.
    pub fn keeps(self) -> bool {
        self != CacheTtl::For(Duration::ZERO)
    }

    /// Whether a stored result this old answers a request with this TTL.
    pub fn answers(self, age: Duration) -> bool {
        match self {
            CacheTtl::Never => true,
            CacheTtl::For(d) => !d.is_zero() && age <= d,
        }
    }

    /// Seconds, with `-1` for never: how it is written down.
    pub fn secs(self) -> i64 {
        match self {
            CacheTtl::Never => -1,
            CacheTtl::For(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        }
    }

    /// The inverse of [`CacheTtl::secs`].
    pub fn from_secs(secs: i64) -> CacheTtl {
        match u64::try_from(secs) {
            Ok(s) => CacheTtl::For(Duration::from_secs(s)),
            Err(_) => CacheTtl::Never,
        }
    }
}

impl std::fmt::Display for CacheTtl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheTtl::Never => f.write_str("never"),
            CacheTtl::For(d) => {
                let s = d.as_secs();
                match s {
                    0 => f.write_str("0"),
                    s if s % 86_400 == 0 => write!(f, "{}d", s / 86_400),
                    s if s % 3600 == 0 => write!(f, "{}h", s / 3600),
                    s if s % 60 == 0 => write!(f, "{}m", s / 60),
                    s => write!(f, "{s}s"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_unit_reads() {
        let secs = |s: u64| Ok(CacheTtl::For(Duration::from_secs(s)));
        assert_eq!(CacheTtl::parse("1800"), secs(1800));
        assert_eq!(CacheTtl::parse("10s"), secs(10));
        assert_eq!(CacheTtl::parse("30m"), secs(1800));
        assert_eq!(CacheTtl::parse("1h"), secs(3600));
        assert_eq!(CacheTtl::parse("10d"), secs(864_000));
        assert_eq!(CacheTtl::parse(" 2h "), secs(7200));
        assert_eq!(CacheTtl::parse("0"), secs(0));
        assert_eq!(CacheTtl::parse("0m"), secs(0));
        assert_eq!(CacheTtl::parse("-1"), Ok(CacheTtl::Never));
    }

    #[test]
    fn what_is_not_a_duration_is_refused() {
        for bad in ["", "h", "-2", "-1h", "1.5h", "1h30m", "1H", "10w", "ten", "1 h", "+5", "99999999999999999999d"] {
            let e = CacheTtl::parse(bad).unwrap_err();
            assert!(e.contains("not a duration"), "{bad}: {e}");
        }
    }

    #[test]
    fn a_number_and_its_text_agree() {
        assert_eq!(CacheTtl::from_json(&serde_json::json!(1800)), CacheTtl::parse("30m"));
        assert_eq!(CacheTtl::from_json(&serde_json::json!(-1)), Ok(CacheTtl::Never));
        assert_eq!(CacheTtl::from_json(&serde_json::json!("1h")), CacheTtl::parse("3600"));
        assert!(CacheTtl::from_json(&serde_json::json!(1.5)).is_err());
        assert!(CacheTtl::from_json(&serde_json::json!(true)).is_err());
        assert_eq!(CacheTtl::from_yaml(&serde_yaml::from_str("30m").unwrap()), CacheTtl::parse("1800"));
        assert_eq!(CacheTtl::from_yaml(&serde_yaml::from_str("-1").unwrap()), Ok(CacheTtl::Never));
    }

    #[test]
    fn a_datasource_that_keeps_nothing_is_not_overridden() {
        let (zero, hour) = (CacheTtl::For(Duration::ZERO), CacheTtl::parse("1h").unwrap());
        assert_eq!(CacheTtl::resolve(None, "wh", None), Ok(CacheTtl::DEFAULT));
        assert_eq!(CacheTtl::resolve(None, "wh", Some(hour)), Ok(hour));
        assert_eq!(CacheTtl::resolve(Some(zero), "wh", Some(hour)), Ok(zero));
        assert_eq!(CacheTtl::resolve(Some(hour), "wh", Some(CacheTtl::Never)), Ok(hour));
        assert_eq!(CacheTtl::resolve(Some(zero), "wh", Some(zero)), Ok(zero));
        assert_eq!(CacheTtl::resolve(None, "wh", Some(zero)), Ok(zero));
        for asked in [hour, CacheTtl::Never] {
            let e = CacheTtl::resolve(Some(asked), "wh", Some(zero)).unwrap_err();
            assert!(e.contains("datasource wh keeps no results"), "{e}");
        }
    }

    #[test]
    fn what_answers() {
        let half_hour = CacheTtl::parse("30m").unwrap();
        assert!(half_hour.answers(Duration::from_secs(1799)));
        assert!(!half_hour.answers(Duration::from_secs(1801)));
        assert!(CacheTtl::Never.answers(Duration::from_secs(u64::MAX / 2)));
        assert!(!CacheTtl::For(Duration::ZERO).answers(Duration::ZERO));
        assert!(!CacheTtl::For(Duration::ZERO).keeps());
        assert_eq!(CacheTtl::from_secs(CacheTtl::Never.secs()), CacheTtl::Never);
        assert_eq!(CacheTtl::from_secs(half_hour.secs()), half_hour);
        assert_eq!(half_hour.to_string(), "30m");
        assert_eq!(CacheTtl::Never.to_string(), "never");
        assert_eq!(CacheTtl::parse("90").unwrap().to_string(), "90s");
    }
}

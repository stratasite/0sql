//! Warehouse dialects as the dwh gem defines them: one `base.yml` of
//! defaults and one override file per adapter, each a flat map of settings.
//! String settings are templates with `@name` placeholders
//! (`DATE_TRUNC('@unit', @exp)`), boolean settings are capabilities.
//!
//! Users author expressions in their warehouse's own SQL; this layer only
//! renders the pieces the planner itself emits.

use serde_yaml::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum DialectError {
    #[error("io {0}: {1}")]
    Io(String, std::io::Error),
    #[error("yaml {0}: {1}")]
    Yaml(String, serde_yaml::Error),
    #[error("dialect {0} has no setting {1}")]
    Missing(String, String),
    #[error("setting {0} is not a {1}")]
    Type(String, &'static str),
    #[error("template {0} needs @{1}")]
    Placeholder(String, String),
    #[error("unknown adapter {0}")]
    Unknown(String),
}

#[derive(Debug, Clone)]
pub struct Dialect {
    name: String,
    settings: BTreeMap<String, Value>,
}

impl Dialect {
    /// `base.yml` overlaid with `<name>.yml` from `dir`.
    pub fn load(dir: &Path, name: &str) -> Result<Self, DialectError> {
        let mut settings = read_yaml(&dir.join("base.yml"))?;
        let own = dir.join(format!("{name}.yml"));
        if !own.exists() {
            return Err(DialectError::Unknown(name.to_string()));
        }
        settings.extend(read_yaml(&own)?);
        Ok(Self { name: name.to_string(), settings })
    }

    /// The same overlay from YAML text, for settings compiled into a binary.
    pub fn from_texts(name: &str, base: &str, own: &str) -> Result<Self, DialectError> {
        let mut settings = parse_yaml(base, "base.yml")?;
        settings.extend(parse_yaml(own, &format!("{name}.yml"))?);
        Ok(Self { name: name.to_string(), settings })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// A copy with per-query overrides applied (the query's `db_settings`).
    /// This dialect with a query's `db_settings` laid over it. Without
    /// any, the dialect itself: a plan shares the catalogue's copy rather
    /// than cloning the whole settings map.
    pub fn with_overrides<'a>(
        self: &std::sync::Arc<Self>,
        overrides: impl IntoIterator<Item = (&'a str, &'a serde_json::Value)>,
    ) -> std::sync::Arc<Self> {
        let mut overrides = overrides.into_iter().peekable();
        if overrides.peek().is_none() {
            return self.clone();
        }
        let mut d = Self::clone(self);
        for (k, v) in overrides {
            d.settings.insert(k.to_string(), json_to_yaml(v));
        }
        std::sync::Arc::new(d)
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.settings.get(key)
    }

    pub fn str(&self, key: &str) -> Result<&str, DialectError> {
        match self.settings.get(key) {
            Some(Value::String(s)) => Ok(s),
            Some(_) => Err(DialectError::Type(key.into(), "string")),
            None => Err(DialectError::Missing(self.name.clone(), key.into())),
        }
    }

    pub fn bool(&self, key: &str) -> Result<bool, DialectError> {
        match self.settings.get(key) {
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(DialectError::Type(key.into(), "bool")),
            None => Err(DialectError::Missing(self.name.clone(), key.into())),
        }
    }

    /// Renders the template stored under `key`, substituting `@name` placeholders.
    pub fn render(&self, key: &str, vars: &[(&str, &str)]) -> Result<String, DialectError> {
        let template = self.str(key)?;
        render_template(template, vars).map_err(|p| DialectError::Placeholder(key.into(), p))
    }

    pub fn quote(&self, ident: &str) -> Result<String, DialectError> {
        self.render("quote", &[("exp", ident)])
    }

    /// A string literal with the value's quotes doubled.
    pub fn string_lit(&self, value: &str) -> Result<String, DialectError> {
        let escaped = value.replace('\'', "''");
        self.render("string_literal", &[("exp", &escaped)])
    }

    // ---- the functions the planner emits, mirroring the dwh gem ----

    pub fn lower_case(&self, exp: &str) -> Result<String, DialectError> {
        self.render("lower_case", &[("exp", exp)])
    }

    pub fn cast(&self, exp: &str, ty: &str) -> Result<String, DialectError> {
        self.render("cast", &[("exp", exp), ("type", ty)])
    }

    pub fn cross_join(&self, relation: &str) -> Result<String, DialectError> {
        self.render("cross_join", &[("relation", relation)])
    }

    /// `DATE_TRUNC('month', x)` cast to DATE above the hour grain; the week
    /// start day template when the warehouse's default week differs.
    pub fn truncate_date(&self, unit: &str, exp: &str) -> Result<String, DialectError> {
        let unit = unit.trim().to_lowercase();
        let res = if unit == "week" && self.adjust_week_start_day() {
            let key = format!("{}_week_start_day", self.str("week_start_day")?.to_lowercase());
            self.render(&key, &[("exp", exp)])?
        } else {
            self.render("truncate_date", &[("unit", &unit), ("exp", exp)])?
        };
        if matches!(unit.as_str(), "millisecond" | "second" | "minute" | "hour") {
            Ok(res)
        } else {
            self.cast(&res, "DATE")
        }
    }

    pub fn adjust_week_start_day(&self) -> bool {
        match (self.str("week_start_day"), self.str("default_week_start_day")) {
            (Ok(a), Ok(b)) => a.trim() != b.trim(),
            _ => false,
        }
    }

    pub fn date_literal(&self, val: &str) -> Result<String, DialectError> {
        self.render("date_literal", &[("val", val)])
    }

    pub fn timestamp_literal(&self, val: &str) -> Result<String, DialectError> {
        self.render("date_time_literal", &[("val", val)])
    }

    /// strftime pattern for date values in this warehouse.
    pub fn date_format(&self) -> &str {
        self.str("date_format").unwrap_or("%Y-%m-%d")
    }

    pub fn date_time_format(&self) -> &str {
        self.str("date_time_format").unwrap_or("%Y-%m-%d %H:%M:%S")
    }

    pub fn array_in_list(&self, exp: &str, list: &str) -> Result<String, DialectError> {
        self.render("array_in_list", &[("exp", exp), ("list", list)])
    }

    pub fn array_exclude_list(&self, exp: &str, list: &str) -> Result<String, DialectError> {
        self.render("array_exclude_list", &[("exp", exp), ("list", list)])
    }

    pub fn array_unnest_join(&self, exp: &str, alias: &str) -> Result<String, DialectError> {
        self.render("array_unnest_join", &[("exp", exp), ("alias", alias)])
    }

    pub fn null_if_zero(&self, exp: &str) -> Result<String, DialectError> {
        self.render("null_if_zero", &[("exp", exp)])
    }

    /// Rails `date_add`; Postgres spells a quarter as three months.
    pub fn date_add(&self, unit: &str, val: i64, exp: &str) -> Result<String, DialectError> {
        let (unit, val) = if self.name == "postgres" && unit.trim().to_lowercase() == "quarter" {
            ("months".to_string(), val * 3)
        } else {
            (unit.to_string(), val)
        };
        self.render("date_add", &[("unit", &unit), ("val", &val.to_string()), ("exp", exp)])
    }

    /// Rails `extract_*`: a date part, or an upper-cased day or month name.
    pub fn extract(&self, part: &str, exp: &str, abbreviate: bool) -> Result<String, DialectError> {
        match part {
            "day_name" | "month_name" => {
                let key = match (part, abbreviate) {
                    ("day_name", true) => "abbreviated_day_name_format",
                    ("day_name", false) => "day_name_format",
                    (_, true) => "abbreviated_month_name_format",
                    _ => "month_name_format",
                };
                let fmt = self.str(key)?.to_string();
                let formatted = self.render("date_format_sql", &[("exp", exp), ("format", &fmt)])?;
                self.render("upper_case", &[("exp", &formatted)])
            }
            other => self.render(&format!("extract_{other}"), &[("exp", exp)]),
        }
    }

    pub fn greedy_apply_date_filters(&self) -> bool {
        self.bool("greedy_apply_date_filters").unwrap_or(false)
    }

    pub fn extend_ending_date_to_last_hour_of_day(&self) -> bool {
        self.bool("extend_ending_date_to_last_hour_of_day").unwrap_or(false)
    }

    pub fn apply_advanced_filtering_on_array_projections(&self) -> bool {
        self.bool("apply_advanced_filtering_on_array_projections").unwrap_or(false)
    }

    pub fn supports_array_functions(&self) -> bool {
        self.bool("supports_array_functions").unwrap_or(false)
    }

    pub fn final_pass_measure_join_type(&self) -> &str {
        self.str("final_pass_measure_join_type").unwrap_or("full")
    }

    fn measure_filtering_strategy(&self) -> &str {
        self.str("cross_universe_measure_filtering_strategy").unwrap_or("both")
    }

    pub fn intermediate_measure_filter(&self) -> bool {
        matches!(self.measure_filtering_strategy(), "both" | "intermediate")
    }

    pub fn final_measure_filter(&self) -> bool {
        matches!(self.measure_filtering_strategy(), "both" | "final")
    }

    /// The SQL keywords an expression parser must not read as columns:
    /// `base.yml`'s list plus the adapter's `extra_reserved_keywords`, lower-cased.
    pub fn reserved_keywords(&self) -> Vec<String> {
        let mut out = Vec::new();
        for key in ["reserved_keywords", "extra_reserved_keywords"] {
            if let Some(Value::Sequence(items)) = self.settings.get(key) {
                for it in items {
                    let s = match it {
                        Value::String(s) => s.to_lowercase(),
                        Value::Bool(b) => b.to_string(),
                        Value::Number(n) => n.to_string(),
                        _ => continue,
                    };
                    if !out.contains(&s) {
                        out.push(s);
                    }
                }
            }
        }
        out
    }

    /// Aggregate function names: `base.yml`'s list plus the adapter's
    /// `extra_aggregate_functions`, lower-cased.
    pub fn aggregate_functions(&self) -> Vec<String> {
        let mut out = Vec::new();
        for key in ["aggregate_functions", "extra_aggregate_functions"] {
            if let Some(Value::Sequence(items)) = self.settings.get(key) {
                for it in items {
                    if let Value::String(s) = it {
                        let s = s.to_lowercase();
                        if !out.contains(&s) {
                            out.push(s);
                        }
                    }
                }
            }
        }
        out
    }

    pub fn supports_cte(&self) -> bool {
        self.bool("supports_common_table_expressions").unwrap_or(false)
    }

    pub fn supports_sub_queries(&self) -> bool {
        self.bool("supports_sub_queries").unwrap_or(false)
    }

    pub fn temp_table_type(&self) -> &str {
        self.str("temp_table_type").unwrap_or("cte")
    }

    pub fn temp_table_prefix(&self) -> &str {
        self.str("temp_table_prefix").unwrap_or("")
    }
}

/// Every adapter that has a settings file in `dir`.
pub struct Dialects {
    source: Source,
    /// Each adapter's settings are read once.
    loaded: std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<Dialect>>>,
}

enum Source {
    Dir(std::path::PathBuf),
    Builtin,
}

/// The dwh gem's adapter settings compiled in, so a binary needs no
/// settings directory. The texts are the fixture copies (MIT licensed).
const BUILTIN: &[(&str, &str)] = &[
    ("athena", include_str!("../../../dialects/athena.yml")),
    ("bigquery", include_str!("../../../dialects/bigquery.yml")),
    ("clickhouse", include_str!("../../../dialects/clickhouse.yml")),
    ("databricks", include_str!("../../../dialects/databricks.yml")),
    ("druid", include_str!("../../../dialects/druid.yml")),
    ("duckdb", include_str!("../../../dialects/duckdb.yml")),
    ("mysql", include_str!("../../../dialects/mysql.yml")),
    ("postgres", include_str!("../../../dialects/postgres.yml")),
    ("redshift", include_str!("../../../dialects/redshift.yml")),
    ("snowflake", include_str!("../../../dialects/snowflake.yml")),
    ("sqlite", include_str!("../../../dialects/sqlite.yml")),
    ("sqlserver", include_str!("../../../dialects/sqlserver.yml")),
    ("trino", include_str!("../../../dialects/trino.yml")),
];
const BUILTIN_BASE: &str = include_str!("../../../dialects/base.yml");

impl Dialects {
    pub fn new(dir: &Path) -> Self {
        Self { source: Source::Dir(dir.to_path_buf()), loaded: Default::default() }
    }

    /// The settings compiled into the binary.
    pub fn builtin() -> Self {
        Self { source: Source::Builtin, loaded: Default::default() }
    }

    /// `base.yml` alone: the reserved keywords and aggregate functions every
    /// warehouse shares, which classify calculations before a datasource is known.
    pub fn base(&self) -> Result<std::sync::Arc<Dialect>, DialectError> {
        self.get("base")
    }

    pub fn get(&self, adapter: &str) -> Result<std::sync::Arc<Dialect>, DialectError> {
        if let Some(d) = self.loaded.lock().unwrap().get(adapter) {
            return Ok(d.clone());
        }
        if adapter == "base" {
            let d = match &self.source {
                Source::Dir(dir) => Dialect { name: "base".into(), settings: read_yaml(&dir.join("base.yml"))? },
                Source::Builtin => Dialect::from_texts("base", BUILTIN_BASE, "")?,
            };
            let d = std::sync::Arc::new(d);
            self.loaded.lock().unwrap().insert(adapter.to_string(), d.clone());
            return Ok(d);
        }
        let d = match &self.source {
            Source::Dir(dir) => Dialect::load(dir, adapter)?,
            Source::Builtin => {
                let own = BUILTIN
                    .iter()
                    .find(|(n, _)| *n == adapter)
                    .map(|(_, t)| *t)
                    .ok_or_else(|| DialectError::Unknown(adapter.to_string()))?;
                Dialect::from_texts(adapter, BUILTIN_BASE, own)?
            }
        };
        let d = std::sync::Arc::new(d);
        self.loaded.lock().unwrap().insert(adapter.to_string(), d.clone());
        Ok(d)
    }

    pub fn names(&self) -> Result<Vec<String>, DialectError> {
        let Source::Dir(dir) = &self.source else {
            return Ok(BUILTIN.iter().map(|(n, _)| n.to_string()).collect());
        };
        let mut names = Vec::new();
        let entries = std::fs::read_dir(dir).map_err(|e| DialectError::Io(dir.display().to_string(), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| DialectError::Io(dir.display().to_string(), e))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("yml") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if stem != "base" {
                        names.push(stem.to_string());
                    }
                }
            }
        }
        names.sort();
        Ok(names)
    }
}

fn read_yaml(path: &Path) -> Result<BTreeMap<String, Value>, DialectError> {
    let text = std::fs::read_to_string(path).map_err(|e| DialectError::Io(path.display().to_string(), e))?;
    parse_yaml(&text, &path.display().to_string())
}

fn parse_yaml(text: &str, label: &str) -> Result<BTreeMap<String, Value>, DialectError> {
    if text.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let value: Value = serde_yaml::from_str(text).map_err(|e| DialectError::Yaml(label.to_string(), e))?;
    let mut map = BTreeMap::new();
    if let Value::Mapping(m) = value {
        for (k, v) in m {
            if let Value::String(k) = k {
                map.insert(k, v);
            }
        }
    }
    Ok(map)
}

fn json_to_yaml(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Number(i.into())
            } else {
                Value::Number(serde_yaml::Number::from(n.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::String(s) => Value::String(s.clone()),
        other => Value::String(other.to_string()),
    }
}

/// Substitutes `@name` placeholders. Identifiers are `[A-Za-z0-9_]+`, read
/// greedily so `@start_exp` is never mistaken for `@start`. Returns the
/// missing placeholder name on failure.
pub fn render_template(template: &str, vars: &[(&str, &str)]) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            if end > start {
                let name = &template[start..end];
                match vars.iter().find(|(k, _)| *k == name) {
                    Some((_, v)) => out.push_str(v),
                    None => return Err(name.to_string()),
                }
                i = end;
                continue;
            }
        }
        let ch = template[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_greedy() {
        let r =
            render_template("DATE_DIFF('@unit', @start_exp, @end_exp)", &[("unit", "day"), ("start_exp", "a"), ("end_exp", "b")]).unwrap();
        assert_eq!(r, "DATE_DIFF('day', a, b)");
    }

    #[test]
    fn missing_placeholder_is_named() {
        assert_eq!(render_template("@exp + @val", &[("exp", "x")]), Err("val".to_string()));
    }

    #[test]
    fn literal_at_survives() {
        assert_eq!(render_template("a @ b", &[]).unwrap(), "a @ b");
    }
}

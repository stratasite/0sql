//! `zsql check` against a catalog: does the model name tables and
//! columns the warehouse has, and types it agrees with? The commonest
//! defect in a hand-written model is a typo in a physical name, and until
//! `zsql catalog` nothing could catch one; now a catalog file beside
//! the model is enough, and no query is run.
//!
//! Folding follows the planner's own rule for identifiers: a bare name
//! matches regardless of case, a name written in the dialect's quotes
//! matches exactly. Getting that wrong would be a wall of false positives
//! on the first run, which is how a checker gets turned off.

use crate::generate::{map_type, Mapped};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use zsql_service::Model;
use zsql_source::{Catalog, Table};

/// Findings per datasource that has a catalog; a datasource without one
/// is listed as unchecked. `fails` is true when a table or column the
/// model names is not in the warehouse.
pub fn findings(model: &Model) -> Value {
    let Some(dir) = model.sources.iter().find(|p| p.file_name().is_some_and(|n| n == "project.yml")).and_then(|p| p.parent()) else {
        return json!({ "unchecked": ["no project directory"] });
    };
    let s = model.universe().snapshot();
    let mut out: Vec<Value> = Vec::new();
    let mut unchecked: Vec<String> = Vec::new();
    for ds in &s.datasources {
        let path = crate::catalog::path(dir, &ds.uid);
        let Ok(text) = std::fs::read_to_string(&path) else {
            unchecked.push(ds.uid.clone());
            continue;
        };
        let Ok(cat) = serde_json::from_str::<Catalog>(&text) else {
            unchecked.push(format!("{} (catalog unreadable)", ds.uid));
            continue;
        };
        let quotes = model
            .dialects
            .get(&ds.adapter)
            .ok()
            .and_then(|d| d.get("quote").and_then(|q| q.as_str()).map(zsql_spec::expr::quote_pair))
            .unwrap_or(("\"".into(), "\"".into()));
        let mut missing_tables = Vec::new();
        let mut missing_columns = Vec::new();
        let mut checked = 0;
        let mut outside = 0;
        // What each field's columns hold, over every table of this
        // datasource, and which tables could not be looked at: a field's
        // declared type is judged against all of its columns at once,
        // not one table at a time, because the model holds one type for
        // the field however many tables map it.
        let mut columns_of: BTreeMap<zsql_semantic::Id, Vec<Value>> = BTreeMap::new();
        let mut unseen: Vec<zsql_semantic::Id> = Vec::new();
        for table in s.tables.iter().filter(|t| t.datasource_id == ds.id) {
            let found = match resolve(&cat, &table.physical_name, &quotes) {
                (Some(found), _) => found,
                (None, true) => {
                    outside += 1;
                    unseen.push(table.id);
                    continue;
                }
                (None, false) => {
                    missing_tables.push(json!({ "table": table.name, "physical_name": table.physical_name }));
                    unseen.push(table.id);
                    continue;
                }
            };
            checked += 1;
            let by_column: BTreeMap<String, &zsql_source::Column> = found.columns.iter().map(|c| (c.name.to_lowercase(), c)).collect();
            for e in s.expressions.iter().filter(|e| e.table_id == table.id) {
                let field = s.fields.iter().find(|f| f.id == e.field_id);
                for col in &e.column_refs {
                    let Some(c) = column(&by_column, found, col, &quotes) else {
                        missing_columns.push(json!({ "table": table.name, "physical_name": table.physical_name, "column": col, "field": field.map(|f| f.name.clone()) }));
                        continue;
                    };
                    // One bare column, so the field's type is the column's.
                    if e.column_refs.len() == 1 && e.sql.as_deref().map(str::trim).is_some_and(|q| q.eq_ignore_ascii_case(col)) {
                        if let (Some(f), Mapped::Known(actual)) = (field, map_type(&c.data_type)) {
                            columns_of
                                .entry(f.id)
                                .or_default()
                                .push(json!({ "table": table.name, "column": c.name, "warehouse": c.data_type, "maps_to": actual }));
                        }
                    }
                }
            }
        }
        // The declared type must be the widest of the columns the field
        // maps to: that one covers them all, and a narrower one claims
        // less than the data holds. A field some of whose tables could
        // not be looked at is left alone when its declaration is wider
        // than what was seen, since an unread table may be the reason.
        let mut type_mismatches = Vec::new();
        for (id, columns) in &columns_of {
            let Some(f) = s.fields.iter().find(|f| f.id == *id) else { continue };
            let Some(declared) = f.data_type.and_then(|d| serde_json::to_value(d).ok()).and_then(|v| v.as_str().map(str::to_string)) else {
                continue;
            };
            let mapped: Vec<&str> = columns.iter().filter_map(|c| c["maps_to"].as_str()).collect();
            if mapped.iter().all(|m| *m == declared) {
                continue;
            }
            let complete = !s.expressions.iter().any(|e| e.field_id == *id && unseen.contains(&(e.table_id)));
            let widest = crate::generate::widest(mapped.iter().copied());
            match widest {
                Some(w) if w == declared => {}
                // Wider than every column seen, and a table was not seen:
                // the declaration may be right for that one.
                Some(w) if !complete && crate::generate::widen(&declared, w) == Some(name_static(&declared)) => {}
                _ => type_mismatches.push(json!({
                    "field": f.name, "declared": declared, "should_be": widest, "columns": columns, "complete": complete,
                })),
            }
        }
        if outside > 0 {
            let scopes: Vec<String> = cat.scopes.iter().map(|s| s.schema.clone().unwrap_or_else(|| "every schema".into())).collect();
            unchecked.push(format!(
                "{} ({outside} table{} outside the catalog's scope {})",
                ds.uid,
                if outside == 1 { "" } else { "s" },
                scopes.join(", ")
            ));
        }
        if let Some(days) = age_days(&cat.read_at).filter(|d| *d >= 7) {
            unchecked.push(format!("{} (catalog read {days} days ago)", ds.uid));
        }
        out.push(json!({
            "datasource": ds.uid, "read_at": cat.read_at, "tables_checked": checked, "scopes": cat.scopes, "added": cat.added,
            "missing_tables": missing_tables, "missing_columns": missing_columns, "type_mismatches": type_mismatches,
        }));
    }
    let fails =
        out.iter().any(|d| !d["missing_tables"].as_array().unwrap().is_empty() || !d["missing_columns"].as_array().unwrap().is_empty());
    json!({ "datasources": out, "unchecked": unchecked, "fails": fails })
}

/// A declared type as the static str the widening tables hold.
fn name_static(t: &str) -> &'static str {
    const ALL: [&str; 8] = ["string", "integer", "bigint", "decimal", "date", "date_time", "boolean", "binary"];
    ALL.iter().find(|x| **x == t).copied().unwrap_or("")
}

/// A name as written in the model: quoted means exact, bare means any case.
fn unquote<'a>(name: &'a str, (open, close): &(String, String)) -> (&'a str, bool) {
    let n = name.trim();
    match n.strip_prefix(open.as_str()).and_then(|r| r.strip_suffix(close.as_str())) {
        Some(inner) => (inner, true),
        None => (n, false),
    }
}

fn same(a: &str, b: &str, exact: bool) -> bool {
    if exact {
        a == b
    } else {
        a.eq_ignore_ascii_case(b)
    }
}

/// A physical name split on the dots outside its quotes: the last part is
/// the table, the one before it the schema, and anything earlier (a
/// database, a catalog, a project) is not the catalog's to know.
pub(crate) fn parts<'a>(physical: &'a str, (open, close): &(String, String)) -> (Option<&'a str>, &'a str) {
    let mut parts: Vec<&str> = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    let bytes = physical.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if physical[i..].starts_with(open.as_str()) && (open != close || depth == 0) {
            depth += 1;
            i += open.len();
            continue;
        }
        if depth > 0 && physical[i..].starts_with(close.as_str()) {
            depth -= 1;
            i += close.len();
            continue;
        }
        if depth == 0 && bytes[i] == b'.' {
            parts.push(&physical[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    parts.push(&physical[start..]);
    let table = parts.pop().unwrap_or(physical);
    let schema = parts.pop().filter(|s| !s.is_empty());
    (schema, table)
}

/// The catalog table a physical name denotes, each part quoted or bare
/// on its own. `None` with `true` means the name is outside the
/// catalog's scope, so nothing can be said about it.
fn resolve<'a>(cat: &'a Catalog, physical: &str, quotes: &(String, String)) -> (Option<&'a Table>, bool) {
    let (schema, table) = parts(physical, quotes);
    let (tname, texact) = unquote(table, quotes);
    // A catalog with no schemas at all (SQLite's) cannot contradict a
    // schema part, so `main.orders` is `orders` there.
    let schema = schema.map(|s| unquote(s, quotes)).filter(|_| cat.tables.iter().any(|t| t.schema.is_some()));
    if !cat.covers(schema.map(|(s, _)| s), tname) {
        return (None, true);
    }
    let found = cat.tables.iter().find(|t| {
        same(&t.name, tname, texact)
            && match schema {
                Some((sname, sexact)) => t.schema.as_deref().is_some_and(|ts| same(ts, sname, sexact)),
                None => true,
            }
    });
    (found, false)
}

fn column<'a>(
    by_lower: &BTreeMap<String, &'a zsql_source::Column>,
    table: &'a Table,
    name: &str,
    quotes: &(String, String),
) -> Option<&'a zsql_source::Column> {
    let (n, exact) = unquote(name, quotes);
    if exact {
        table.columns.iter().find(|c| c.name == n)
    } else {
        by_lower.get(&n.to_lowercase()).copied()
    }
}

/// Prints the findings; true when nothing is missing.
pub fn print(v: &Value) -> bool {
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    for d in v["datasources"].as_array().into_iter().flatten() {
        let ds = s(&d["datasource"]);
        for t in d["missing_tables"].as_array().into_iter().flatten() {
            println!("catalog {ds}: table {} names {}, which the warehouse does not have", s(&t["table"]), s(&t["physical_name"]));
        }
        for c in d["missing_columns"].as_array().into_iter().flatten() {
            println!(
                "catalog {ds}: {} has no column {} ({} on {})",
                s(&c["physical_name"]),
                s(&c["column"]),
                s(&c["field"]),
                s(&c["table"])
            );
        }
        for m in d["type_mismatches"].as_array().into_iter().flatten() {
            let columns = m["columns"].as_array().cloned().unwrap_or_default();
            let where_: Vec<String> = columns.iter().map(|c| format!("{} on {}", s(&c["warehouse"]), s(&c["table"]))).collect();
            let advice = match m["should_be"].as_str() {
                Some(w) => format!("declare {w}"),
                None => "those are different kinds of thing, so one of them is wrong".to_string(),
            };
            println!(
                "catalog {ds}: {} says {}, and its column{} {}: {advice}",
                s(&m["field"]),
                s(&m["declared"]),
                if columns.len() == 1 { " is" } else { "s are" },
                where_.join(", ")
            );
        }
        let n = d["tables_checked"].as_u64().unwrap_or(0);
        println!(
            "catalog {ds}: {n} table{} checked against catalog read {}",
            if n == 1 { "" } else { "s" },
            s(&d["read_at"]).get(..10).unwrap_or("")
        );
    }
    for u in v["unchecked"].as_array().into_iter().flatten() {
        let u = s(u);
        if u.contains("outside the catalog's scope") {
            let ds = u.split(' ').next().unwrap_or("");
            println!("catalog: {u}; `zsql catalog read -d {ds} --schema <name>` or `zsql catalog read -d {ds} <table>` would read them");
        } else if u.contains("catalog read") {
            let ds = u.split(' ').next().unwrap_or("");
            println!("catalog: {u}; `zsql catalog sync -d {ds}` re-reads what it records");
        } else {
            let ds = u.split(' ').next().unwrap_or("");
            println!("catalog: {u} not checked; `zsql catalog read -d {ds} --schema <name>` reads one");
        }
    }
    !v["fails"].as_bool().unwrap_or(false)
}

/// Whole days since an RFC 3339 timestamp, by the date alone.
fn age_days(read_at: &str) -> Option<i64> {
    let date = read_at.get(..10)?;
    let mut parts = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (parts.next()??, parts.next()??, parts.next()??);
    let days_from_civil = |y: i64, m: i64, d: i64| -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let mp = if m > 2 { m - 3 } else { m + 9 };
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    };
    let then = days_from_civil(y, m, d);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64 / 86_400;
    Some(now - then)
}

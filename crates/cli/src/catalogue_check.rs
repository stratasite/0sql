//! `zsql check` against a catalogue: does the model name tables and
//! columns the warehouse has, and types it agrees with? The commonest
//! defect in a hand-written model is a typo in a physical name, and until
//! `zsql introspect` nothing could catch one; now a catalogue file beside
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
use zsql_source::{Catalogue, Table};

/// Findings per datasource that has a catalogue; a datasource without one
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
        let path = crate::introspect::path(dir, &ds.uid);
        let Ok(text) = std::fs::read_to_string(&path) else {
            unchecked.push(ds.uid.clone());
            continue;
        };
        let Ok(cat) = serde_json::from_str::<Catalogue>(&text) else {
            unchecked.push(format!("{} (catalogue unreadable)", ds.uid));
            continue;
        };
        let quotes = model
            .dialects
            .get(&ds.adapter)
            .ok()
            .and_then(|d| d.get("quote").and_then(|q| q.as_str()).map(quote_pair))
            .unwrap_or(("\"".into(), "\"".into()));
        let mut missing_tables = Vec::new();
        let mut missing_columns = Vec::new();
        let mut type_mismatches = Vec::new();
        let mut checked = 0;
        for table in s.tables.iter().filter(|t| t.datasource_id == ds.id) {
            let Some(found) = resolve(&cat, &table.physical_name, &quotes) else {
                missing_tables.push(json!({ "table": table.name, "physical_name": table.physical_name }));
                continue;
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
                        if let (Some(f), Some(declared)) = (field, field.and_then(|f| f.data_type)) {
                            if let Mapped::Known(actual) = map_type(&c.data_type) {
                                let declared =
                                    serde_json::to_value(declared).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
                                if declared != actual {
                                    type_mismatches.push(json!({ "table": table.name, "field": f.name, "column": c.name, "declared": declared, "warehouse": c.data_type, "maps_to": actual }));
                                }
                            }
                        }
                    }
                }
            }
        }
        out.push(json!({
            "datasource": ds.uid, "read_at": cat.read_at, "tables_checked": checked,
            "missing_tables": missing_tables, "missing_columns": missing_columns, "type_mismatches": type_mismatches,
        }));
    }
    let fails =
        out.iter().any(|d| !d["missing_tables"].as_array().unwrap().is_empty() || !d["missing_columns"].as_array().unwrap().is_empty());
    json!({ "datasources": out, "unchecked": unchecked, "fails": fails })
}

/// The dialect's quote characters, from its template: `"@exp"` gives
/// `"` and `"`, `[@exp]` gives `[` and `]`.
fn quote_pair(template: &str) -> (String, String) {
    match template.split_once("@exp") {
        Some((o, c)) if !o.is_empty() && !c.is_empty() => (o.to_string(), c.to_string()),
        _ => ("\"".into(), "\"".into()),
    }
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

/// The catalogue table a physical name denotes: `schema.table` or `table`,
/// each part quoted or bare on its own.
fn resolve<'a>(cat: &'a Catalogue, physical: &str, quotes: &(String, String)) -> Option<&'a Table> {
    let (schema, table) = match physical.rsplit_once('.') {
        Some((s, t)) if !s.is_empty() => (Some(s), t),
        _ => (None, physical),
    };
    let (tname, texact) = unquote(table, quotes);
    let schema = schema.map(|s| unquote(s, quotes));
    cat.tables.iter().find(|t| {
        same(&t.name, tname, texact)
            && match schema {
                Some((sname, sexact)) => t.schema.as_deref().is_some_and(|ts| same(ts, sname, sexact)),
                None => true,
            }
    })
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
            println!("catalogue {ds}: table {} names {}, which the warehouse does not have", s(&t["table"]), s(&t["physical_name"]));
        }
        for c in d["missing_columns"].as_array().into_iter().flatten() {
            println!(
                "catalogue {ds}: {} has no column {} ({} on {})",
                s(&c["physical_name"]),
                s(&c["column"]),
                s(&c["field"]),
                s(&c["table"])
            );
        }
        for m in d["type_mismatches"].as_array().into_iter().flatten() {
            println!(
                "catalogue {ds}: {} says {} but {}.{} is {}, which is {}",
                s(&m["field"]),
                s(&m["declared"]),
                s(&m["table"]),
                s(&m["column"]),
                s(&m["warehouse"]),
                s(&m["maps_to"])
            );
        }
        let n = d["tables_checked"].as_u64().unwrap_or(0);
        println!(
            "catalogue {ds}: {n} table{} checked against catalogue read {}",
            if n == 1 { "" } else { "s" },
            s(&d["read_at"]).get(..10).unwrap_or("")
        );
    }
    for u in v["unchecked"].as_array().into_iter().flatten() {
        let u = s(u);
        println!("catalogue: {u} not checked; `zsql introspect {}` would make that possible", u.split(' ').next().unwrap_or(""));
    }
    !v["fails"].as_bool().unwrap_or(false)
}

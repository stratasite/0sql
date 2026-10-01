//! The shorthand: one line of comma-separated items, each read as a
//! projection, a filter or a calculation from its shape, turned into the
//! JSON spec the server plans.
//!
//! ```text
//! item category, month(date), store net paid desc, item category in (Books, Music)
//! yoy(store net paid), date between 2024-01-01 and 2024-12-31
//! Share = sum([Store Net Paid]@m) / sum([Catalog Net Paid]@m)
//! ```
//!
//! A filter has a predicate: `=`, `!=`, `>`, `>=`, `<`, `<=`, `in (...)`,
//! `not in (...)`, `between a and b`, `contains`, `starts with`, `ends with`
//! (each with `not`), `is null`, `is not null`, `top N [by measure]`.
//! A calculation is a formula holding a bracket reference (`[Name]@m`,
//! `[Name]@d`, `[Alias]`), named with `Alias = formula` or `formula as
//! Alias`; unnamed, the formula is its own alias. Anything else is a
//! projection: a field name or uid wrapped in decorator functions
//! (`month(...)`, `yoy(...)`, `pct(...)`, `running_sum(...)`,
//! `moving_avg(..., 7)`), with an optional `as Alias` and `asc`/`desc`.
//! A name in brackets or quotes is a field and nothing else.

use serde_json::{json, Map, Value};

/// Splits on commas outside quotes, brackets and parentheses.
pub fn split_items(line: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut cur = String::new();
    let (mut depth, mut quote): (i32, Option<char>) = (0, None);
    for ch in line.chars() {
        match quote {
            Some(q) => {
                cur.push(ch);
                if ch == q {
                    quote = None;
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    cur.push(ch);
                }
                '(' | '[' => {
                    depth += 1;
                    cur.push(ch);
                }
                ')' | ']' => {
                    depth -= 1;
                    cur.push(ch);
                }
                ',' if depth <= 0 => items.push(std::mem::take(&mut cur)),
                _ => cur.push(ch),
            },
        }
    }
    items.push(cur);
    items.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

#[derive(Debug, Clone)]
pub enum Item {
    Projection(Value),
    Filter(Value),
    Calculation(Value),
}

/// The line as a spec, with what each item was read as.
pub fn parse_line(line: &str) -> Result<(Value, Vec<Item>), String> {
    let mut items = Vec::new();
    for text in split_items(line) {
        items.push(parse_item(&text)?);
    }
    if items.is_empty() {
        return Err("nothing to plan".into());
    }
    let mut spec = Map::new();
    let (mut projections, mut calculations, mut filters) = (Vec::new(), Vec::new(), Vec::new());
    for it in &items {
        match it {
            Item::Projection(p) => projections.push(p.clone()),
            Item::Calculation(c) => calculations.push(c.clone()),
            Item::Filter(f) => filters.push(f.clone()),
        }
    }
    spec.insert("projections".into(), Value::Array(projections));
    if !calculations.is_empty() {
        spec.insert("calculations".into(), Value::Array(calculations));
    }
    if !filters.is_empty() {
        spec.insert("filters".into(), Value::Array(filters));
    }
    Ok((Value::Object(spec), items))
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && ((s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"'))) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn list_values(inner: &str) -> String {
    split_items(inner).iter().map(|v| unquote(v)).collect::<Vec<_>>().join(", ")
}

/// A field written as `[Name]`, `[Name]@m`, `"Name"` or `'Name'`.
fn escaped_field(s: &str) -> Option<String> {
    let s = s.trim();
    let (body, suffix) = match s.rsplit_once('@') {
        Some((b, k)) if b.ends_with(']') && matches!(k.to_lowercase().as_str(), "d" | "m" | "dim" | "measure") => (b, format!("@{k}")),
        _ => (s, String::new()),
    };
    let inner = if body.len() >= 2 && body.starts_with('[') && body.ends_with(']') {
        &body[1..body.len() - 1]
    } else if suffix.is_empty() && body.len() >= 2 && ((body.starts_with('"') && body.ends_with('"')) || (body.starts_with('\'') && body.ends_with('\''))) {
        &body[1..body.len() - 1]
    } else {
        return None;
    };
    let inner = inner.trim();
    (!inner.is_empty() && !inner.contains(['[', ']', '"', '\''])).then(|| format!("{inner}{suffix}"))
}

fn field_text(s: &str) -> String {
    escaped_field(s).unwrap_or_else(|| s.trim().to_string())
}

/// Lower-cased with the inside of every bracketed or quoted span blanked,
/// so keyword searches never look inside a field name.
fn masked(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut closer: Option<char> = None;
    for ch in text.chars() {
        match closer {
            Some(c) if ch == c => {
                closer = None;
                out.push(ch);
            }
            Some(_) => out.extend(std::iter::repeat('_').take(ch.len_utf8())),
            None => {
                closer = match ch {
                    '[' => Some(']'),
                    '"' => Some('"'),
                    '\'' => Some('\''),
                    _ => None,
                };
                out.extend(ch.to_lowercase());
            }
        }
    }
    if out.len() == text.len() { out } else { text.to_lowercase() }
}

pub fn parse_item(text: &str) -> Result<Item, String> {
    let text = text.trim();
    if let Some(field) = escaped_field(text) {
        return Ok(Item::Projection(json!({ "field": field })));
    }
    let lower = masked(text);

    if let Some(eq) = lower.find(":=").or_else(|| lower.find('=')) {
        let width = if lower[eq..].starts_with(":=") { 2 } else { 1 };
        let (alias, formula) = (text[..eq].trim(), text[eq + width..].trim());
        if formula.contains('[') && !alias.is_empty() && !alias.contains(|c: char| "<>!=".contains(c)) && escaped_field(alias).is_none() {
            return Ok(Item::Calculation(json!({ "alias": alias, "sql": formula, "calculation": true })));
        }
    }

    let filter = |field: &str, predicate: &str, value: Option<String>, value_end: Option<String>, top_n_measure: Option<String>| -> Result<Item, String> {
        let field = field_text(field);
        if field.is_empty() {
            return Err(format!("'{text}': a filter needs a field before its predicate"));
        }
        let mut f = json!({ "field": field, "predicate": predicate });
        if let Some(v) = value {
            f["value"] = Value::String(v);
        }
        if let Some(v) = value_end {
            f["value_end"] = Value::String(v);
        }
        if let Some(m) = top_n_measure {
            f["top_n_measure"] = Value::String(m);
        }
        Ok(Item::Filter(f))
    };
    if let Some(field) = lower.strip_suffix(" is not null") {
        return filter(&text[..field.len()], "is_not_null", None, None, None);
    }
    if let Some(field) = lower.strip_suffix(" is null") {
        return filter(&text[..field.len()], "is_null", None, None, None);
    }
    for (kw, predicate) in [(" not in ", "exclude_list"), (" in ", "in_list")] {
        if let Some(pos) = lower.find(kw) {
            let rest = text[pos + kw.len()..].trim();
            let inner = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')')).unwrap_or(rest);
            return filter(&text[..pos], predicate, Some(list_values(inner)), None, None);
        }
        let paren = if kw == " not in " { " not in(" } else { " in(" };
        if let Some(pos) = lower.find(paren) {
            let inner = text[pos + paren.len() - 1..].trim().trim_start_matches('(').trim_end_matches(')');
            return filter(&text[..pos], predicate, Some(list_values(inner)), None, None);
        }
    }
    if let Some(pos) = lower.find(" between ") {
        let rest = &text[pos + 9..];
        let Some(and) = rest.to_lowercase().find(" and ") else {
            return Err(format!("'{text}': between needs `a and b`"));
        };
        return filter(&text[..pos], "between", Some(unquote(&rest[..and])), Some(unquote(&rest[and + 5..])), None);
    }
    if let Some(pos) = lower.find(" top ") {
        let rest = text[pos + 5..].trim();
        let (n, measure) = match rest.to_lowercase().find(" by ") {
            Some(by) => (rest[..by].trim(), Some(rest[by + 4..].trim().to_string())),
            None => (rest, None),
        };
        if n.parse::<i64>().is_err() {
            return Err(format!("'{text}': top needs a count, as in `category top 10 by revenue`"));
        }
        return filter(&text[..pos], "top_n", Some(n.to_string()), None, measure);
    }
    for (kw, yes, no) in [(" contains ", "contains", "does_not_contain"), (" starts with ", "starts_with", "does_not_start_with"), (" ends with ", "ends_with", "does_not_end_with")] {
        if let Some(pos) = lower.find(kw) {
            let field = &text[..pos];
            let value = unquote(&text[pos + kw.len()..]);
            let field_l = field.to_lowercase();
            return match field_l.strip_suffix(" not").or_else(|| field_l.strip_suffix(" does not")) {
                Some(f) => filter(&field[..f.len()], no, Some(value), None, None),
                None => filter(field, yes, Some(value), None, None),
            };
        }
    }
    for (op, predicate) in [(">=", "greater_than_or_equal_to"), ("<=", "less_than_or_equal_to"), ("!=", "does_not_equal"), ("<>", "does_not_equal"), ("=", "equals"), (">", "greater_than"), ("<", "less_than")] {
        if let Some(pos) = lower.find(op) {
            return filter(&text[..pos], predicate, Some(unquote(&text[pos + op.len()..])), None, None);
        }
    }

    match parse_projection(text) {
        Ok(item) => Ok(item),
        Err(e) if text.contains('[') => {
            let (formula, alias) = match lower.find(" as ") {
                Some(pos) => (text[..pos].trim(), field_text(&text[pos + 4..])),
                None => (text, text.to_string()),
            };
            let _ = e;
            Ok(Item::Calculation(json!({ "alias": alias, "sql": formula, "calculation": true })))
        }
        Err(e) => Err(e),
    }
}

fn parse_projection(text: &str) -> Result<Item, String> {
    let mut body = text.to_string();
    let mut order_by = None;
    for (suffix, dir) in [(" desc", "desc"), (" asc", "asc")] {
        if masked(&body).ends_with(suffix) {
            order_by = Some(dir);
            body.truncate(body.len() - suffix.len());
            body = body.trim().to_string();
        }
    }
    let mut alias = None;
    if let Some(pos) = masked(&body).find(" as ") {
        alias = Some(field_text(&body[pos + 4..]));
        body.truncate(pos);
    }
    let mut decorators = Vec::new();
    let mut inner = body.trim().to_string();
    loop {
        if escaped_field(&inner).is_some() {
            break;
        }
        let Some(paren) = inner.find('(') else { break };
        if !inner.ends_with(')') {
            break;
        }
        let func = inner[..paren].trim().to_lowercase();
        let args = inner[paren + 1..inner.len() - 1].to_string();
        let mut parts = split_items(&args);
        if parts.is_empty() {
            return Err(format!("'{text}': {func}() needs a field"));
        }
        let field = parts.remove(0);
        let dec = decorator(&func, &parts).ok_or_else(|| format!("'{text}': unknown function {func}(); see .help"))?;
        decorators.push(dec);
        inner = field;
    }
    decorators.reverse();
    let inner = field_text(&inner);
    if inner.is_empty() {
        return Err(format!("'{text}': a projection needs a field"));
    }
    let mut p = json!({ "field": inner });
    if let Some(a) = alias {
        p["alias"] = Value::String(a);
    }
    if let Some(o) = order_by {
        p["order_by"] = Value::String(o.into());
    }
    if !decorators.is_empty() {
        p["decorators"] = Value::Array(decorators);
    }
    Ok(Item::Projection(p))
}

fn decorator(func: &str, args: &[String]) -> Option<Value> {
    let d = match func {
        "raw" | "millisecond" | "second" | "minute" | "hour" | "day" | "week" | "month" | "quarter" | "year" => json!({ "type": "truncate", "grain": func }),
        "dow" | "dom" | "doy" | "woy" | "hr" | "min" | "mon" | "qtr" | "yr" | "day_name" | "month_name" | "year_month" | "day_of_week" | "day_of_month" | "day_of_year" | "week_of_year" => {
            let part = match func {
                "dow" => "day_of_week",
                "dom" => "day_of_month",
                "doy" => "day_of_year",
                "woy" => "week_of_year",
                "hr" => "hour",
                "min" => "minute",
                "mon" => "month",
                "qtr" => "quarter",
                "yr" => "year",
                other => other,
            };
            json!({ "type": "extract", "extract": part })
        }
        "yoy" | "qoq" | "mom" | "wow" | "dod" | "yoy_pct" | "qoq_pct" | "mom_pct" | "wow_pct" | "dod_pct" => {
            let (base, pct) = match func.strip_suffix("_pct") {
                Some(b) => (b, true),
                None => (func, false),
            };
            let transform = match base {
                "yoy" => "year_over_year",
                "qoq" => "quarter_over_quarter",
                "mom" => "month_over_month",
                "wow" => "week_over_week",
                _ => "day_over_day",
            };
            let mut d = json!({ "type": "temporalize", "transform": transform });
            if pct {
                d["percent_change"] = Value::Bool(true);
            }
            d
        }
        "pct" | "share" | "contribute" | "contribution" => {
            let mut d = json!({ "type": "contribute" });
            if !args.is_empty() {
                d["partition_refs"] = Value::Array(args.iter().map(|s| Value::String(s.clone())).collect());
            }
            d
        }
        _ => {
            let (mode, function) = func.split_once('_')?;
            if !matches!(mode, "running" | "moving") {
                return None;
            }
            let mut d = json!({ "type": "window", "mode": mode, "function": function });
            if let Some(size) = args.first().and_then(|s| s.trim().parse::<i64>().ok()) {
                d["size"] = Value::from(size);
            }
            let partitions: Vec<Value> = args.iter().filter(|s| s.trim().parse::<i64>().is_err()).map(|s| Value::String(s.clone())).collect();
            if !partitions.is_empty() {
                d["partition_refs"] = Value::Array(partitions);
            }
            d
        }
    };
    Some(d)
}

/// One line per item, for echoing what was understood.
pub fn describe(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .map(|it| match it {
            Item::Projection(p) => {
                let field = p["field"].as_str().unwrap_or("?").to_string();
                let decs = p["decorators"].as_array().cloned().unwrap_or_default();
                let mut s = decs.iter().rev().fold(field, |acc, d| {
                    let detail: Vec<String> = d.as_object().into_iter().flatten().filter(|(k, _)| *k != "type" && *k != "partition_refs").map(|(k, v)| format!("{k}={}", v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect();
                    format!("{}({acc}{})", d["type"].as_str().unwrap_or("?"), if detail.is_empty() { String::new() } else { format!("; {}", detail.join(", ")) })
                });
                if let Some(a) = p["alias"].as_str() {
                    s.push_str(&format!(" as {a}"));
                }
                if let Some(o) = p["order_by"].as_str() {
                    s.push_str(&format!(" {o}"));
                }
                format!("projection   {s}")
            }
            Item::Filter(f) => {
                let field = f["field"].as_str().unwrap_or("?");
                let v = f["value"].as_str().unwrap_or("");
                let tail = match f["predicate"].as_str().unwrap_or("") {
                    "between" => format!("{v} and {}", f["value_end"].as_str().unwrap_or("")),
                    "top_n" => format!("{v}{}", f["top_n_measure"].as_str().map(|m| format!(" by {m}")).unwrap_or_default()),
                    "is_null" | "is_not_null" => String::new(),
                    _ => v.to_string(),
                };
                format!("filter       {field} {} {tail}", f["predicate"].as_str().unwrap_or("")).trim_end().to_string()
            }
            Item::Calculation(c) => format!("calculation  {} = {}", c["alias"].as_str().unwrap_or("?"), c["sql"].as_str().unwrap_or("")),
        })
        .collect()
}

pub const HELP: &str = "\
Type a comma-separated line. Each item is read from its shape:
  projection   item category | month(date) | yoy(store net paid) | store net paid as Paid desc
               functions: raw hour day week month quarter year (truncate); dow dom doy woy month_name (extract);
               yoy qoq mom wow dod [+_pct] (period over period); pct(x) (share of total);
               running_sum(x) moving_avg(x, 7) and other running_/moving_ windows
  filter       item category = Books | store net paid > 100 | date between 2024-01-01 and 2024-12-31
               category in (Books, Music) | category not in (Toys) | name contains foo | name not starts with x
               date is null | category top 10 by store net paid
  calculation  Share = sum([Store Net Paid]@m) / sum([Catalog Net Paid]@m) | max([gross sales]@m) as Peak
               a formula with a [Name]@m, [Name]@d or [Alias] reference; unnamed, the formula is its alias
Fields are names or uids, matched without regard to case; add @d or @m when a name is both.
[Name] or \"Name\" is always a field, whatever the name says: [Tables], \"Sales in Store\" > 5.
Commands start with a dot, as in sqlite: .explain <line or {...spec...}>, .drill <line> (dimensions the query can add),
.fields [text], .tables, .values <dim>,
.json {...spec...}, .spec <line> (the spec a line makes), .branch [name], .help, .quit";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_are_detected() {
        let (spec, items) = parse_line("item category, yoy(month(date)) as Growth desc, store net paid > 100, category in (Books, Music), Share = sum([A]@m) / sum([B]@m), date between 2024-01-01 and 2024-12-31, cat top 5 by revenue, name is not null, max([gross sales]@m) as Peak, [Tables]").unwrap();
        assert_eq!(spec["projections"].as_array().unwrap().len(), 3);
        assert_eq!(spec["calculations"].as_array().unwrap().len(), 2);
        assert_eq!(spec["filters"].as_array().unwrap().len(), 5);
        match &items[1] {
            Item::Projection(p) => {
                assert_eq!(p["alias"], "Growth");
                assert_eq!(p["order_by"], "desc");
                assert_eq!(p["decorators"][0]["type"], "truncate");
                assert_eq!(p["decorators"][1]["type"], "temporalize");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&items[3], Item::Filter(f) if f["predicate"] == "in_list" && f["value"] == "Books, Music"));
        assert!(matches!(&items[6], Item::Filter(f) if f["predicate"] == "top_n" && f["top_n_measure"] == "revenue"));
        assert!(matches!(&items[8], Item::Calculation(c) if c["alias"] == "Peak"));
        assert!(matches!(&items[9], Item::Projection(p) if p["field"] == "Tables"));
        assert!(parse_item("frob(date)").is_err());
    }
}

//! The REPL's shorthand: one line of comma-separated items, each read as a
//! projection, a filter or a calculation from its shape.
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
//! projection: a field name or uid, wrapped in decorator functions
//! (`month(...)`, `yoy(...)`, `pct(...)`, `running_sum(...)`,
//! `moving_avg(..., 7)`), with an optional `as Alias` and `asc`/`desc`.
//!
//! A name in brackets or quotes is a field and nothing else: `[Tables]`,
//! `"Sales in Store" > 5`, `month("Date")`. Keywords inside it are not
//! read, so a field can be called anything.

use serde_json::{json, Value};
use zsql_spec::spec::{DecoratorSpec, FilterLeafSpec, ProjectionSpec, Spec};

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
                ',' if depth <= 0 => {
                    items.push(std::mem::take(&mut cur));
                }
                _ => cur.push(ch),
            },
        }
    }
    items.push(cur);
    items.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// What one item was read as, for echoing back.
#[derive(Debug, Clone)]
pub enum Item {
    Projection(ProjectionSpec),
    Filter(FilterLeafSpec),
    Calculation(ProjectionSpec),
}

pub fn parse_line(line: &str) -> Result<(Spec, Vec<Item>), String> {
    let mut items = Vec::new();
    for text in split_items(line) {
        items.push(parse_item(&text)?);
    }
    if items.is_empty() {
        return Err("nothing to plan".into());
    }
    let mut spec = Spec::default();
    let mut filters = Vec::new();
    for it in &items {
        match it {
            Item::Projection(p) => spec.projections.push(p.clone()),
            Item::Calculation(c) => spec.calculations.push(c.clone()),
            Item::Filter(f) => filters.push(serde_json::to_value(f).unwrap()),
        }
    }
    if !filters.is_empty() {
        spec.filters = Some(Value::Array(filters));
    }
    Ok((spec, items))
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && ((s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"'))) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// A field written as `[Name]`, `[Name]@m`, `"Name"` or `'Name'`: the
/// name inside, keeping a `@d`/`@m` kind suffix.
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

/// A field as written, with any brackets or quotes removed.
fn field_text(s: &str) -> String {
    escaped_field(s).unwrap_or_else(|| s.trim().to_string())
}

/// The lower-cased text with the inside of every bracketed or quoted span
/// blanked out, so keyword and operator searches never look inside a
/// field name; positions line up with the original.
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
    // Lower-casing can change byte length for a few scripts; keep the
    // mask aligned with the original in that case by falling back.
    if out.len() == text.len() { out } else { text.to_lowercase() }
}

fn list_values(inner: &str) -> String {
    split_items(inner).iter().map(|v| unquote(v)).collect::<Vec<_>>().join(", ")
}

pub fn parse_item(text: &str) -> Result<Item, String> {
    let text = text.trim();
    // An escaped name on its own is a projection of that field, whatever it says.
    if let Some(field) = escaped_field(text) {
        return Ok(Item::Projection(ProjectionSpec { field: Some(Value::String(field)), ..Default::default() }));
    }
    let lower = masked(text);

    // Calculation: `Alias = formula with [refs]` (also `Alias := formula`).
    if let Some(eq) = lower.find(":=").or_else(|| lower.find('=')) {
        let width = if lower[eq..].starts_with(":=") { 2 } else { 1 };
        let (alias, formula) = (text[..eq].trim(), text[eq + width..].trim());
        if formula.contains('[') && !alias.is_empty() && !alias.contains(|c: char| "<>!=".contains(c)) && escaped_field(alias).is_none() {
            return Ok(Item::Calculation(ProjectionSpec { alias: Some(alias.to_string()), sql: Some(formula.to_string()), calculation: true, ..Default::default() }));
        }
    }

    // Filters, longest keyword forms first.
    let filter = |field: &str, predicate: &str, value: Option<String>, value_end: Option<String>, top_n_measure: Option<String>| -> Result<Item, String> {
        let field = field_text(field);
        if field.is_empty() {
            return Err(format!("'{text}': a filter needs a field before its predicate"));
        }
        Ok(Item::Filter(FilterLeafSpec {
            field: Value::String(field),
            predicate: predicate.to_string(),
            value: value.map(Value::String),
            value_end: value_end.map(Value::String),
            top_n_measure: top_n_measure.map(Value::String),
            ..Default::default()
        }))
    };
    if let Some(field) = lower.strip_suffix(" is not null") {
        return filter(&text[..field.len()], "is_not_null", None, None, None);
    }
    if let Some(field) = lower.strip_suffix(" is null") {
        return filter(&text[..field.len()], "is_null", None, None, None);
    }
    for (kw, predicate) in [(" not in ", "exclude_list"), (" in ", "in_list")] {
        if let Some(pos) = find_keyword(&lower, kw) {
            let rest = text[pos + kw.len()..].trim();
            let inner = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')')).unwrap_or(rest);
            return filter(&text[..pos], predicate, Some(list_values(inner)), None, None);
        }
        if kw == " not in " {
            if let Some(pos) = find_keyword(&lower, " not in(") {
                let inner = text[pos + 7..].trim().trim_start_matches('(').trim_end_matches(')');
                return filter(&text[..pos], predicate, Some(list_values(inner)), None, None);
            }
        } else if let Some(pos) = find_keyword(&lower, " in(") {
            let inner = text[pos + 3..].trim().trim_start_matches('(').trim_end_matches(')');
            return filter(&text[..pos], predicate, Some(list_values(inner)), None, None);
        }
    }
    if let Some(pos) = find_keyword(&lower, " between ") {
        let rest = &text[pos + 9..];
        let rl = rest.to_lowercase();
        let Some(and) = find_keyword(&rl, " and ") else {
            return Err(format!("'{text}': between needs `a and b`"));
        };
        return filter(&text[..pos], "between", Some(unquote(&rest[..and])), Some(unquote(&rest[and + 5..])), None);
    }
    if let Some(pos) = find_keyword(&lower, " top ") {
        let rest = text[pos + 5..].trim();
        let (n, measure) = match find_keyword(&rest.to_lowercase(), " by ") {
            Some(by) => (rest[..by].trim(), Some(rest[by + 4..].trim().to_string())),
            None => (rest, None),
        };
        if n.parse::<i64>().is_err() {
            return Err(format!("'{text}': top needs a count, as in `category top 10 by revenue`"));
        }
        return filter(&text[..pos], "top_n", Some(n.to_string()), None, measure);
    }
    for (kw, yes, no) in [
        (" contains ", "contains", "does_not_contain"),
        (" starts with ", "starts_with", "does_not_start_with"),
        (" ends with ", "ends_with", "does_not_end_with"),
    ] {
        if let Some(pos) = find_keyword(&lower, kw) {
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

    // A projection when the decorator functions are known; otherwise a
    // formula with a bracket reference is a calculation, `as Alias` naming
    // it and the formula being its own alias when unnamed.
    match parse_projection(text) {
        Ok(item) => Ok(item),
        Err(e) if text.contains('[') => {
            let (formula, alias) = match find_keyword(&lower, " as ") {
                Some(pos) => (text[..pos].trim(), field_text(&text[pos + 4..])),
                None => (text, text.to_string()),
            };
            let _ = e;
            Ok(Item::Calculation(ProjectionSpec { alias: Some(alias), sql: Some(formula.to_string()), calculation: true, ..Default::default() }))
        }
        Err(e) => Err(e),
    }
}

fn parse_projection(text: &str) -> Result<Item, String> {
    let mut body = text.to_string();
    let mut order_by = None;
    for (suffix, dir) in [(" desc", "desc"), (" asc", "asc")] {
        if masked(&body).ends_with(suffix) {
            order_by = Some(dir.to_string());
            body.truncate(body.len() - suffix.len());
            body = body.trim().to_string();
        }
    }
    let mut alias = None;
    if let Some(pos) = find_keyword(&masked(&body), " as ") {
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
        let dec = decorator(&func, &parts).ok_or_else(|| format!("'{text}': unknown function {func}(); see help"))?;
        decorators.push(dec);
        inner = field;
    }
    // Innermost applies first (truncate before yoy), which is the order they
    // were nested in: reverse to list the outer ones last.
    decorators.reverse();
    let inner = field_text(&inner);
    if inner.is_empty() {
        return Err(format!("'{text}': a projection needs a field"));
    }
    Ok(Item::Projection(ProjectionSpec { field: Some(Value::String(inner)), alias, order_by, decorators, ..Default::default() }))
}

/// A keyword with word boundaries on both sides (the patterns carry their spaces).
fn find_keyword(lower: &str, kw: &str) -> Option<usize> {
    lower.find(kw)
}

fn decorator(func: &str, args: &[String]) -> Option<DecoratorSpec> {
    let attrs = |pairs: &[(&str, Value)]| pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    let d = match func {
        "raw" | "millisecond" | "second" | "minute" | "hour" | "day" | "week" | "month" | "quarter" | "year" => DecoratorSpec { kind: "truncate".into(), attrs: attrs(&[("grain", json!(func))]) },
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
            DecoratorSpec { kind: "extract".into(), attrs: attrs(&[("extract", json!(part))]) }
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
            let mut a = attrs(&[("transform", json!(transform))]);
            if pct {
                a.insert("percent_change".into(), json!(true));
            }
            DecoratorSpec { kind: "temporalize".into(), attrs: a }
        }
        "pct" | "share" | "contribute" | "contribution" => {
            let mut a = attrs(&[]);
            if !args.is_empty() {
                a.insert("partition_refs".into(), Value::Array(args.iter().map(|s| Value::String(s.clone())).collect()));
            }
            DecoratorSpec { kind: "contribute".into(), attrs: a }
        }
        _ => {
            let (mode, function) = func.split_once('_')?;
            if !matches!(mode, "running" | "moving") {
                return None;
            }
            let mut a = attrs(&[("mode", json!(mode)), ("function", json!(function))]);
            if let Some(size) = args.first().and_then(|s| s.trim().parse::<i64>().ok()) {
                a.insert("size".into(), json!(size));
            }
            let partitions: Vec<Value> = args.iter().filter(|s| s.trim().parse::<i64>().is_err()).map(|s| Value::String(s.clone())).collect();
            if !partitions.is_empty() {
                a.insert("partition_refs".into(), Value::Array(partitions));
            }
            DecoratorSpec { kind: "window".into(), attrs: a }
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
                let field = p.field.as_ref().and_then(Value::as_str).unwrap_or("?").to_string();
                let mut s = p.decorators.iter().rev().fold(field, |acc, d| {
                    let detail: Vec<String> = d.attrs.iter().filter(|(k, _)| *k != "partition_refs").map(|(k, v)| format!("{k}={}", v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect();
                    format!("{}({acc}{})", d.kind, if detail.is_empty() { String::new() } else { format!("; {}", detail.join(", ")) })
                });
                if let Some(a) = &p.alias {
                    s.push_str(&format!(" as {a}"));
                }
                if let Some(o) = &p.order_by {
                    s.push_str(&format!(" {o}"));
                }
                format!("projection   {s}")
            }
            Item::Filter(f) => {
                let field = f.field.as_str().unwrap_or("?");
                let v = f.value.as_ref().and_then(Value::as_str).unwrap_or("");
                let tail = match f.predicate.as_str() {
                    "between" => format!("{v} and {}", f.value_end.as_ref().and_then(Value::as_str).unwrap_or("")),
                    "top_n" => format!("{v}{}", f.top_n_measure.as_ref().and_then(Value::as_str).map(|m| format!(" by {m}")).unwrap_or_default()),
                    "is_null" | "is_not_null" => String::new(),
                    _ => v.to_string(),
                };
                format!("filter       {field} {} {tail}", f.predicate).trim_end().to_string()
            }
            Item::Calculation(c) => format!("calculation  {} = {}", c.alias.as_deref().unwrap_or("?"), c.sql.as_deref().unwrap_or("")),
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
Commands start with a dot, as in sqlite: .fields [text], .tables, .json {...spec...}, .tojson <line>, .help, .quit\n.tojson prints a shorthand line as the JSON spec it stands for, without planning it";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_split_outside_parens_and_quotes() {
        assert_eq!(split_items("a, month(b), c in (x, y), 'p, q'"), vec!["a", "month(b)", "c in (x, y)", "'p, q'"]);
    }

    #[test]
    fn shapes_are_detected() {
        let (spec, items) = parse_line("item category, yoy(month(date)) as Growth desc, store net paid > 100, category in (Books, Music), Share = sum([A]@m) / sum([B]@m), date between 2024-01-01 and 2024-12-31, cat top 5 by revenue, name is not null").unwrap();
        assert_eq!(spec.projections.len(), 2);
        assert_eq!(spec.calculations.len(), 1);
        assert_eq!(spec.filters.as_ref().unwrap().as_array().unwrap().len(), 5);
        match &items[1] {
            Item::Projection(p) => {
                assert_eq!(p.alias.as_deref(), Some("Growth"));
                assert_eq!(p.order_by.as_deref(), Some("desc"));
                assert_eq!(p.decorators.iter().map(|d| d.kind.as_str()).collect::<Vec<_>>(), vec!["truncate", "temporalize"]);
            }
            other => panic!("{other:?}"),
        }
        match &items[3] {
            Item::Filter(f) => {
                assert_eq!(f.predicate, "in_list");
                assert_eq!(f.value, Some(Value::String("Books, Music".into())));
            }
            other => panic!("{other:?}"),
        }
        match &items[6] {
            Item::Filter(f) => {
                assert_eq!(f.predicate, "top_n");
                assert_eq!(f.top_n_measure, Some(Value::String("revenue".into())));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&items[7], Item::Filter(f) if f.predicate == "is_not_null"));
    }

    #[test]
    fn escaped_names_are_fields_whatever_they_say() {
        assert!(matches!(parse_item("[Tables]"), Ok(Item::Projection(p)) if p.field == Some(Value::String("Tables".into()))));
        assert!(matches!(parse_item("\"Sales in Store\" > 5"), Ok(Item::Filter(f)) if f.field == Value::String("Sales in Store".into()) && f.predicate == "greater_than"));
        assert!(matches!(parse_item("month([Date between])"), Ok(Item::Projection(p)) if p.field == Some(Value::String("Date between".into())) && p.decorators.len() == 1));
        assert!(matches!(parse_item("[Net Paid] as \"Paid\" desc"), Ok(Item::Projection(p)) if p.alias.as_deref() == Some("Paid") && p.order_by.as_deref() == Some("desc")));
        // a calculation's formula keeps its brackets
        assert!(matches!(parse_item("Share = [A]@m / [B]@m"), Ok(Item::Calculation(c)) if c.sql.as_deref() == Some("[A]@m / [B]@m")));
        // a bare formula is a calculation named after itself, or after `as`
        assert!(matches!(parse_item("max([gross sales]@m)"), Ok(Item::Calculation(c)) if c.alias.as_deref() == Some("max([gross sales]@m)")));
        assert!(matches!(parse_item("max([gross sales]@m) as Peak"), Ok(Item::Calculation(c)) if c.alias.as_deref() == Some("Peak") && c.sql.as_deref() == Some("max([gross sales]@m)")));
        // a kind suffix on an escaped name stays a field reference
        assert!(matches!(parse_item("[gross sales]@m > 100"), Ok(Item::Filter(f)) if f.field == Value::String("gross sales@m".into())));
        assert!(matches!(parse_item("[Date]@d"), Ok(Item::Projection(p)) if p.field == Some(Value::String("Date@d".into()))));
    }

    #[test]
    fn unknown_function_is_refused() {
        assert!(parse_item("frob(date)").is_err());
        assert!(matches!(parse_item("moving_avg(paid, 7)"), Ok(Item::Projection(p)) if p.decorators[0].attrs["size"] == json!(7)));
    }
}

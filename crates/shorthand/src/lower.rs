//! From the parsed items to the JSON spec the server plans, and the echo
//! of how each item was read. The lowering has no model: names are
//! emitted as written and the spec lowering resolves them.

use crate::parse::{Expr, Field, FilterNode, Item, ItemKind, Kind, Leaf, Pred, Projection, Segment, SegmentItem, SegmentKind, Val};
use serde_json::{json, Map, Value};

pub fn to_spec(items: &[Item]) -> Result<Value, String> {
    let mut spec = Map::new();
    let (mut projections, mut calculations, mut filters, mut segments, mut hints) = (vec![], vec![], vec![], vec![], vec![]);
    let mut limit = None;
    let mut raws: Vec<Map<String, Value>> = vec![];
    for it in items {
        match &it.kind {
            ItemKind::Projection(p) => {
                let (mut v, is_calc) = projection(p)?;
                attach(&mut v, it.raw.as_ref());
                if is_calc {
                    calculations.push(v);
                } else {
                    projections.push(v);
                }
            }
            ItemKind::Filter(f) => {
                let mut v = filter_node(f);
                attach(&mut v, it.raw.as_ref());
                filters.push(v);
            }
            ItemKind::Segment(s) => {
                let mut v = segment(s)?;
                attach(&mut v, it.raw.as_ref());
                segments.push(v);
            }
            ItemKind::Limit(n) => {
                if limit.replace(*n).is_some() {
                    return Err("limit is given twice".into());
                }
            }
            ItemKind::Via(names) => hints.extend(names.iter().map(|f| Value::String(f.name.clone()))),
            ItemKind::Raw(m) => raws.push(m.clone()),
        }
    }
    spec.insert("projections".into(), Value::Array(projections));
    if !calculations.is_empty() {
        spec.insert("calculations".into(), Value::Array(calculations));
    }
    if !filters.is_empty() {
        spec.insert("filters".into(), filters_value(filters));
    }
    if !segments.is_empty() {
        spec.insert("segments".into(), Value::Array(segments));
    }
    if let Some(n) = limit {
        spec.insert("limit".into(), json!(n));
    }
    if !hints.is_empty() {
        spec.insert("hints".into(), Value::Array(hints));
    }
    let mut spec = Value::Object(spec);
    for raw in raws {
        merge(&mut spec, Value::Object(raw));
    }
    Ok(spec)
}

fn attach(target: &mut Value, raw: Option<&Map<String, Value>>) {
    if let Some(raw) = raw {
        merge(target, Value::Object(raw.clone()));
    }
}

/// Deep for objects, append for arrays, replace for scalars.
fn merge(target: &mut Value, extra: Value) {
    match (target, extra) {
        (Value::Object(t), Value::Object(e)) => {
            for (k, v) in e {
                match t.get_mut(&k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        t.insert(k, v);
                    }
                }
            }
        }
        (Value::Array(t), Value::Array(e)) => t.extend(e),
        (t, e) => *t = e,
    }
}

fn field_ref(f: &Field, into: &mut Map<String, Value>) {
    into.insert("field".into(), Value::String(f.name.clone()));
    if let Some(k) = f.kind {
        into.insert("field_type".into(), Value::String(kind_name(k).into()));
    }
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Dimension => "dimension",
        Kind::Measure => "measure",
    }
}

/// A projection's spec object, and whether it is a calculation.
fn projection(p: &Projection) -> Result<(Value, bool), String> {
    let mut decorators = Vec::new();
    let mut expr = &p.expr;
    loop {
        match expr {
            Expr::Ref(_) | Expr::Calc { .. } => break,
            Expr::Extract { part, inner } => {
                decorators.push(json!({ "type": "extract", "extract": part }));
                expr = inner;
            }
            Expr::Custom { sql, inner } => {
                decorators.push(json!({ "type": "customize", "custom_sql": sql }));
                expr = inner;
            }
            Expr::Deco { func, inner, n, by, order } => {
                decorators.push(decorator(func, *n, by, order));
                expr = inner;
            }
        }
    }
    decorators.reverse();
    let mut m = Map::new();
    let is_calc = match expr {
        Expr::Ref(f) => {
            field_ref(f, &mut m);
            false
        }
        Expr::Calc { sql, data_type } => {
            m.insert("calculation".into(), Value::Bool(true));
            m.insert("alias".into(), Value::String(p.alias.clone().unwrap_or_else(|| sql.clone())));
            m.insert("sql".into(), Value::String(sql.clone()));
            if let Some(t) = data_type {
                m.insert("data_type".into(), Value::String(t.clone()));
            }
            true
        }
        _ => unreachable!(),
    };
    if !is_calc {
        if let Some(a) = &p.alias {
            m.insert("alias".into(), Value::String(a.clone()));
        }
    }
    if let Some(o) = &p.order {
        m.insert("order_by".into(), Value::String(o.clone()));
    }
    if !decorators.is_empty() {
        m.insert("decorators".into(), Value::Array(decorators));
    }
    Ok((Value::Object(m), is_calc))
}

fn decorator(func: &str, n: Option<i64>, by: &[Field], order: &[(Field, String)]) -> Value {
    let refs = |fs: &[Field]| Value::Array(fs.iter().map(|f| Value::String(f.name.clone())).collect());
    if crate::parse::TRUNCATES.contains(&func) {
        return json!({ "type": "truncate", "grain": func });
    }
    if let Some(base) = func.strip_suffix("_pct").filter(|b| crate::parse::TEMPORALS.contains(b)).or(Some(func)) {
        if crate::parse::TEMPORALS.contains(&base) {
            let transform = match base {
                "yoy" => "year_over_year",
                "qoq" => "quarter_over_quarter",
                "mom" => "month_over_month",
                "wow" => "week_over_week",
                _ => "day_over_day",
            };
            let mut d = json!({ "type": "temporalize", "transform": transform });
            if func.ends_with("_pct") {
                d["percent_change"] = Value::Bool(true);
            }
            return d;
        }
    }
    if func == "share" || func == "share_filtered" {
        let mut d = json!({ "type": "contribute" });
        if !by.is_empty() {
            d["partition_refs"] = refs(by);
        }
        if func == "share_filtered" {
            d["ignore_partition_filters"] = Value::Bool(false);
        }
        return d;
    }
    let (mode, function) = match func.split_once('_') {
        Some((m @ ("running" | "moving"), f)) if crate::parse::FRAME_FUNCTIONS.contains(&f) => (m, f),
        _ => ("running", func),
    };
    let mut d = json!({ "type": "window", "mode": mode, "function": function });
    match (function, n) {
        ("ntile", Some(b)) => d["buckets"] = json!(b),
        ("lag" | "lead", Some(o)) => d["offset"] = json!(o),
        (_, Some(rows)) if mode == "moving" => d["size"] = json!(rows - 1),
        _ => {}
    }
    if !by.is_empty() {
        d["partition_refs"] = refs(by);
    }
    if !order.is_empty() {
        let mut m = Map::new();
        for (f, dir) in order {
            m.insert(f.name.clone(), Value::String(dir.clone()));
        }
        d["order_by"] = Value::Object(m);
    }
    d
}

/// A value as the spec carries it: a number or a boolean when the text
/// is wholly one and was not quoted, otherwise the text.
fn scalar(v: &Val) -> Value {
    if !v.quoted {
        if let Ok(n) = v.text.parse::<i64>() {
            if !v.text.starts_with('0') || v.text == "0" {
                return json!(n);
            }
        }
        let decimal = v.text.strip_prefix('-').unwrap_or(&v.text);
        if let Some((a, b)) = decimal.split_once('.') {
            if !a.is_empty() && !b.is_empty() && a.chars().all(|c| c.is_ascii_digit()) && b.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(f) = v.text.parse::<f64>() {
                    return json!(f);
                }
            }
        }
        if v.text.eq_ignore_ascii_case("true") {
            return Value::Bool(true);
        }
        if v.text.eq_ignore_ascii_case("false") {
            return Value::Bool(false);
        }
    }
    Value::String(v.text.clone())
}

fn joined(values: &[Val]) -> Value {
    Value::String(values.iter().map(|v| v.text.as_str()).collect::<Vec<_>>().join(", "))
}

fn leaf(l: &Leaf) -> Value {
    let mut m = Map::new();
    field_ref(&l.field, &mut m);
    let (predicate, value, value_end, top_by): (&str, Option<Value>, Option<Value>, Option<&Field>) = match &l.pred {
        Pred::Cmp { op, value } => (op, Some(scalar(value)), None, None),
        Pred::In { negated: false, values } => ("in_list", Some(joined(values)), None, None),
        Pred::In { negated: true, values } => ("exclude_list", Some(joined(values)), None, None),
        Pred::Between { low, high } => ("between", Some(scalar(low)), Some(scalar(high)), None),
        Pred::Like { which, negated, value } => {
            let p = match (which, negated) {
                (&"contains", false) => "contains",
                (&"contains", true) => "does_not_contain",
                (&"starts_with", false) => "starts_with",
                (&"starts_with", true) => "does_not_start_with",
                (_, false) => "ends_with",
                (_, true) => "does_not_end_with",
            };
            (p, Some(scalar(value)), None, None)
        }
        Pred::Null { negated: false } => ("is_null", None, None, None),
        Pred::Null { negated: true } => ("is_not_null", None, None, None),
        Pred::Top { n, by } => ("top_n", Some(json!(n)), None, by.as_ref()),
        Pred::Keyword { values } => ("keyword", Some(joined(values)), None, None),
    };
    m.insert("predicate".into(), Value::String(predicate.into()));
    if let Some(v) = value {
        m.insert("value".into(), v);
    }
    if let Some(v) = value_end {
        m.insert("value_end".into(), v);
    }
    if let Some(f) = top_by {
        m.insert("top_n_measure".into(), Value::String(f.name.clone()));
    }
    Value::Object(m)
}

fn filter_node(f: &FilterNode) -> Value {
    match f {
        FilterNode::Leaf(l) => leaf(l),
        FilterNode::And(kids) => json!({ "and": kids.iter().map(filter_node).collect::<Vec<_>>() }),
        FilterNode::Or(kids) => json!({ "or": kids.iter().map(filter_node).collect::<Vec<_>>() }),
    }
}

/// Items joined by AND: the flat array when every node is a leaf or an
/// AND of leaves, which is the spec's common shape, else the tree.
fn filters_value(nodes: Vec<Value>) -> Value {
    let is_leaf = |v: &Value| v.get("field").is_some();
    let flat = nodes.iter().all(|n| is_leaf(n) || n.get("and").and_then(Value::as_array).is_some_and(|k| k.iter().all(is_leaf)));
    if flat {
        let mut out = Vec::new();
        for n in nodes {
            match n {
                Value::Object(mut m) if m.contains_key("and") => {
                    out.extend(m.remove("and").and_then(|v| v.as_array().cloned()).unwrap_or_default())
                }
                other => out.push(other),
            }
        }
        return Value::Array(out);
    }
    if nodes.len() == 1 {
        return nodes.into_iter().next().unwrap();
    }
    json!({ "and": nodes })
}

fn segment(s: &Segment) -> Result<Value, String> {
    let mut m = Map::new();
    if let Some(n) = &s.name {
        m.insert("name".into(), Value::String(n.clone()));
    }
    let (mut keys, mut measures, mut filters) = (vec![], vec![], vec![]);
    for it in &s.items {
        match it {
            SegmentItem::Key(f) => keys.push(Value::String(f.name.clone())),
            SegmentItem::Measure(f) => measures.push(Value::String(f.name.clone())),
            SegmentItem::Filter(f) => filters.push(filter_node(f)),
        }
    }
    match s.kind {
        SegmentKind::Include => {
            m.insert("mode".into(), json!("include"));
        }
        SegmentKind::Exclude => {
            m.insert("mode".into(), json!("exclude"));
        }
        SegmentKind::Expand => {}
    }
    m.insert("keys".into(), Value::Array(keys));
    if !measures.is_empty() {
        m.insert("measures".into(), Value::Array(measures));
    }
    if !filters.is_empty() {
        m.insert("filters".into(), filters_value(filters));
    }
    if !s.apply_to.is_empty() {
        m.insert("apply_to".into(), Value::Array(s.apply_to.iter().map(|f| Value::String(f.name.clone())).collect()));
    }
    if !s.carried.is_empty() {
        let carried: Vec<Value> = s
            .carried
            .iter()
            .map(|(f, alias)| {
                let mut e = json!({ "field": f.name });
                if let Some(a) = alias {
                    e["alias"] = Value::String(a.clone());
                }
                e
            })
            .collect();
        m.insert("expanding".into(), Value::Array(carried));
    }
    if let Some(j) = &s.join {
        m.insert("join".into(), Value::String(j.clone()));
    }
    if let Some(d) = &s.dedupe {
        m.insert("pair_dedupe".into(), Value::String(d.clone()));
    }
    Ok(Value::Object(m))
}

// ------------------------------------------------------------- describe

/// One line per item: what it was read as, then the item as the
/// grammar would write it back.
pub fn describe(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .map(|it| {
            let (kind, body) = match &it.kind {
                ItemKind::Projection(p) => {
                    let kind = if matches!(root(&p.expr), Expr::Calc { .. }) { "calculation" } else { "projection" };
                    let mut s = expr(&p.expr);
                    if let Some(a) = &p.alias {
                        s.push_str(&format!(" as {}", name(a)));
                    }
                    if let Some(o) = &p.order {
                        s.push_str(&format!(" {o}"));
                    }
                    (kind, s)
                }
                ItemKind::Filter(f) => ("filter", filter(f, false)),
                ItemKind::Segment(s) => ("segment", segment_text(s)),
                ItemKind::Limit(n) => ("setting", format!("limit {n}")),
                ItemKind::Via(names) => ("setting", format!("via ({})", names.iter().map(field).collect::<Vec<_>>().join(", "))),
                ItemKind::Raw(m) => ("spec", Value::Object(m.clone()).to_string()),
            };
            let raw = it.raw.as_ref().map(|m| format!(" {}", Value::Object(m.clone()))).unwrap_or_default();
            format!("{kind:<12} {body}{raw}")
        })
        .collect()
}

fn root(e: &Expr) -> &Expr {
    match e {
        Expr::Extract { inner, .. } | Expr::Custom { inner, .. } | Expr::Deco { inner, .. } => root(inner),
        other => other,
    }
}

fn field(f: &Field) -> String {
    let suffix = match f.kind {
        Some(Kind::Dimension) => "@d",
        Some(Kind::Measure) => "@m",
        None => "",
    };
    format!("{}{suffix}", name(&f.name))
}

/// A name as the grammar would need it written: bare when it can be,
/// quoted when a word of it is a keyword somewhere or a character of it
/// is structural.
fn name(n: &str) -> String {
    const KEYWORDS: [&str; 21] = [
        "and", "or", "not", "in", "is", "between", "top", "contains", "starts", "ends", "keyword", "as", "asc", "desc", "by", "with",
        "for", "left", "inner", "dedupe", "null",
    ];
    let lower = n.to_ascii_lowercase();
    let plain = !n.starts_with('\'')
        && !lower.split_whitespace().collect::<Vec<_>>().windows(2).any(|w| w == ["order", "by"])
        && !n.chars().any(|c| ",()[]{}@=<>!\"".contains(c))
        && !n.split_whitespace().any(|w| KEYWORDS.iter().any(|k| w.eq_ignore_ascii_case(k)))
        && n.split_whitespace().collect::<Vec<_>>().join(" ") == n;
    if plain {
        n.to_string()
    } else {
        format!("\"{}\"", n.replace('"', "\"\""))
    }
}

fn expr(e: &Expr) -> String {
    match e {
        Expr::Ref(f) => field(f),
        Expr::Calc { sql, data_type } => match data_type {
            Some(t) => format!("calc({sql}, {t})"),
            None => format!("calc({sql})"),
        },
        Expr::Extract { part, inner } => format!("extract({part}, {})", expr(inner)),
        Expr::Custom { sql, inner } => format!("custom('{}', {})", sql.replace('\'', "''"), expr(inner)),
        Expr::Deco { func, inner, n, by, order } => {
            let mut s = format!("{func}({}", expr(inner));
            if let Some(n) = n {
                s.push_str(&format!(", {n}"));
            }
            if !by.is_empty() {
                s.push_str(&format!(" by {}", by.iter().map(field).collect::<Vec<_>>().join(", ")));
            }
            if !order.is_empty() {
                s.push_str(&format!(" order by {}", order.iter().map(|(f, d)| format!("{} {d}", field(f))).collect::<Vec<_>>().join(", ")));
            }
            s.push(')');
            s
        }
    }
}

fn val(v: &Val) -> String {
    if v.quoted {
        format!("'{}'", v.text.replace('\'', "''"))
    } else {
        v.text.clone()
    }
}

fn filter(f: &FilterNode, nested: bool) -> String {
    match f {
        FilterNode::Leaf(l) => {
            let fld = field(&l.field);
            match &l.pred {
                Pred::Cmp { op, value } => {
                    let sym = match *op {
                        "greater_than_or_equal_to" => ">=",
                        "less_than_or_equal_to" => "<=",
                        "does_not_equal" => "!=",
                        "equals" => "=",
                        "greater_than" => ">",
                        _ => "<",
                    };
                    format!("{fld} {sym} {}", val(value))
                }
                Pred::In { negated, values } => {
                    format!("{fld} {}in ({})", if *negated { "not " } else { "" }, values.iter().map(val).collect::<Vec<_>>().join(", "))
                }
                Pred::Between { low, high } => format!("{fld} between {} and {}", val(low), val(high)),
                Pred::Like { which, negated, value } => {
                    let w = match *which {
                        "contains" => "contains",
                        "starts_with" => "starts with",
                        _ => "ends with",
                    };
                    format!("{fld} {}{w} {}", if *negated { "not " } else { "" }, val(value))
                }
                Pred::Null { negated } => format!("{fld} is {}null", if *negated { "not " } else { "" }),
                Pred::Top { n, by } => format!("{fld} top {n}{}", by.as_ref().map(|b| format!(" by {}", field(b))).unwrap_or_default()),
                Pred::Keyword { values } => format!("{fld} keyword ({})", values.iter().map(val).collect::<Vec<_>>().join(", ")),
            }
        }
        FilterNode::And(kids) => {
            let s = kids.iter().map(|k| filter(k, true)).collect::<Vec<_>>().join(" and ");
            if nested {
                format!("({s})")
            } else {
                s
            }
        }
        FilterNode::Or(kids) => {
            let s = kids.iter().map(|k| filter(k, true)).collect::<Vec<_>>().join(" or ");
            if nested {
                format!("({s})")
            } else {
                s
            }
        }
    }
}

fn segment_text(s: &Segment) -> String {
    let word = match s.kind {
        SegmentKind::Include => "include",
        SegmentKind::Exclude => "exclude",
        SegmentKind::Expand => "expand",
    };
    let items: Vec<String> = s
        .items
        .iter()
        .map(|it| match it {
            SegmentItem::Key(f) => field(f),
            SegmentItem::Measure(f) => format!("{}@m", f.name),
            SegmentItem::Filter(f) => filter(f, false),
        })
        .collect();
    let mut out = format!("{word} ({})", items.join(", "));
    if !s.carried.is_empty() {
        let c: Vec<String> =
            s.carried.iter().map(|(f, a)| a.as_ref().map(|a| format!("{} as {}", field(f), name(a))).unwrap_or_else(|| field(f))).collect();
        out.push_str(&format!(" with ({})", c.join(", ")));
    }
    if let Some(j) = &s.join {
        out.push_str(&format!(" {j}"));
    }
    if let Some(d) = &s.dedupe {
        out.push_str(&format!(" dedupe {d}"));
    }
    if !s.apply_to.is_empty() {
        out.push_str(&format!(" for ({})", s.apply_to.iter().map(field).collect::<Vec<_>>().join(", ")));
    }
    if let Some(n) = &s.name {
        out.push_str(&format!(" as {}", name(n)));
    }
    out
}

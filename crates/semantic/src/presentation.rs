//! How a value is shown: a field's or a projection's `format` and
//! `display_type`. The engine carries both and applies neither. It checks
//! them where they are written, keeps them in one shape, and sends them
//! out with each result column. A client formats a value first and then
//! renders the formatted text by its display type.
//!
//! A template is text with `{{name}}` slots. Written on a field, the only
//! slot is `{{value}}`, the value being shown: the model does not know
//! which other columns a query will carry. Written on a projection, a slot
//! may also name another column of the same query, as the result names it.
//! See docs/presentation.md.

use serde_json::{Map, Value};

/// Where a format or display type is written, which decides what its
/// templates may read.
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    /// On a field of the model: `{{value}}` only.
    Field,
    /// On a projection: `{{value}}` or one of the query's column names.
    Query(&'a [String]),
}

/// The slot every template may read: the value being shown.
pub const VALUE: &str = "value";

/// A format's types. `raw` says "no format", which on a projection clears
/// the field's.
pub const FORMAT_TYPES: &[&str] = &["raw", "number", "currency", "percent", "date", "datetime", "template"];

/// The display types the engine knows, and which data types each takes
/// (`None` for any). `none` clears a field's display type on a
/// projection. A type starting `x-` is the client's own and passes
/// through unchecked.
pub const DISPLAY_TYPES: &[(&str, Option<&[&str]>)] = &[
    ("none", None),
    ("link", None),
    ("email", Some(&["string"])),
    ("phone", None),
    ("image", Some(&["string"])),
    ("html", Some(&["string"])),
    ("markdown", Some(&["string"])),
    ("transcript", Some(&["string"])),
    ("json", Some(&["string"])),
];

/// The options each display type takes; each is a template.
fn display_options(kind: &str) -> &'static [&'static str] {
    match kind {
        "link" => &["href", "label"],
        "image" => &["alt"],
        _ => &[],
    }
}

/// The shortcut string a format may be written as: `currency:2`,
/// `number:0:abbreviate`, `percent:1`, `date:short`, `datetime:iso`,
/// `template:https://example.com/{{value}}`.
pub fn format_shortcut(syntax: &str) -> Result<Option<Value>, String> {
    let syntax = syntax.trim();
    if syntax.is_empty() {
        return Ok(None);
    }
    let parts: Vec<&str> = syntax.split(':').collect();
    let mut f = Map::new();
    let precision = |p: Option<&&str>| -> Result<Option<i64>, String> {
        match p.copied().filter(|s| !s.is_empty() && *s != "abbreviate") {
            None => Ok(None),
            Some(s) => precision_of(&Value::from(s)).map(Some),
        }
    };
    let kind = parts[0].trim().to_lowercase();
    match kind.as_str() {
        "raw" => {}
        "number" => {
            if let Some(p) = precision(parts.get(1))? {
                f.insert("precision".into(), Value::from(p));
            }
            if parts.contains(&"abbreviate") {
                f.insert("abbreviate".into(), Value::from(true));
            }
        }
        "currency" => {
            f.insert("precision".into(), Value::from(precision(parts.get(1))?.unwrap_or(2)));
            if let Some(unit) = parts.get(2).filter(|s| !s.is_empty() && **s != "abbreviate") {
                f.insert("unit".into(), Value::from(*unit));
            }
            if parts.contains(&"abbreviate") {
                f.insert("abbreviate".into(), Value::from(true));
            }
        }
        "percent" => {
            f.insert("precision".into(), Value::from(precision(parts.get(1))?.unwrap_or(2)));
        }
        "date" | "datetime" => {
            let style = parts[1..].join(":");
            let pattern = match (kind.as_str(), style.as_str()) {
                ("date", "short") => "%b %d, %Y",
                ("date", "long") => "%B %d, %Y",
                ("date", "iso" | "") => "%Y-%m-%d",
                (_, "short") => "%b %d, %Y %I:%M %p",
                (_, "long") => "%B %d, %Y at %I:%M:%S %p",
                (_, "iso" | "") => "%Y-%m-%d %H:%M:%S",
                (_, pattern) => pattern,
            };
            f.insert("pattern".into(), Value::from(pattern));
        }
        "template" => {
            f.insert("template".into(), Value::from(parts[1..].join(":")));
        }
        "html" | "javascript" => return Err(gone(&kind)),
        other => return Err(format!("format '{other}' is not a format; one of {}", FORMAT_TYPES.join(", "))),
    }
    f.insert("type".into(), Value::from(kind));
    Ok(Some(Value::Object(f)))
}

fn gone(kind: &str) -> String {
    match kind {
        "html" => "format type html is gone: write type: template with the markup, and display_type: html to render it".into(),
        _ => "format type javascript is not supported: a model does not carry code for its clients to run; use type: template".into(),
    }
}

/// A format as written, a shortcut string or a mapping, in its one shape:
/// `type` and the options that type takes, blanks dropped, precision an
/// integer, abbreviate kept only when true, template slots spelled as the
/// columns they read. `None` when nothing is said.
pub fn format(v: &Value, scope: Scope) -> Result<Option<Value>, String> {
    let m = match v {
        Value::Null => return Ok(None),
        Value::String(s) => match format_shortcut(s)? {
            Some(Value::Object(m)) => m,
            _ => return Ok(None),
        },
        Value::Object(m) => m.clone(),
        other => return Err(format!("a format is a shortcut string or a mapping, not {other}")),
    };
    let kind = match m.get("type") {
        Some(Value::String(t)) if !t.trim().is_empty() => t.trim().to_lowercase(),
        _ => return Err("a format needs a type".into()),
    };
    let allowed: &[&str] = match kind.as_str() {
        "raw" => &[],
        "number" => &["precision", "abbreviate", "delimiter", "separator", "units"],
        "currency" => &["precision", "abbreviate", "delimiter", "separator", "units", "unit"],
        "percent" => &["precision"],
        "date" | "datetime" => &["pattern"],
        "template" => &["template"],
        "html" | "javascript" => return Err(gone(&kind)),
        other => return Err(format!("format type '{other}' is not a format; one of {}", FORMAT_TYPES.join(", "))),
    };
    let mut out = Map::new();
    out.insert("type".into(), Value::from(kind.clone()));
    for (k, v) in m {
        if k == "type" || v.is_null() || matches!(&v, Value::String(s) if s.is_empty() && k != "template") {
            continue;
        }
        if !allowed.contains(&k.as_str()) {
            return Err(if allowed.is_empty() {
                format!("format {kind} takes no options, and '{k}' is one")
            } else {
                format!("format {kind} takes {}, not '{k}'", allowed.join(", "))
            });
        }
        let v = match k.as_str() {
            "precision" => Value::from(precision_of(&v)?),
            "abbreviate" => match truth(&v) {
                Some(true) => Value::from(true),
                Some(false) => continue,
                None => return Err(format!("format {kind}: abbreviate is true or false, not {v}")),
            },
            "units" => match &v {
                Value::Object(u) if u.values().all(Value::is_string) => v.clone(),
                _ => return Err(format!("format {kind}: units maps a unit name to its suffix, as thousand: K")),
            },
            "template" => match &v {
                Value::String(t) if !t.trim().is_empty() => Value::from(template(t, scope)?),
                _ => return Err("format template needs its template text".into()),
            },
            _ => match &v {
                Value::String(_) => v.clone(),
                _ => return Err(format!("format {kind}: {k} is text, not {v}")),
            },
        };
        out.insert(k, v);
    }
    if kind == "template" && !out.contains_key("template") {
        return Err("format template needs its template text".into());
    }
    Ok(Some(Value::Object(out)))
}

fn precision_of(v: &Value) -> Result<i64, String> {
    let n = match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    };
    n.filter(|n| (0..=20).contains(n)).ok_or_else(|| format!("a format's precision is a whole number of digits from 0 to 20, not {v}"))
}

fn truth(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// A display type as written, a name or a mapping with a `type`, in its
/// one shape: a mapping. `data_type` is the column's, for the types that
/// render text only. `url` and `phone_number`, strata's names, read as
/// `link` and `phone`. `default` says nothing, so a projection that
/// writes it keeps the field's display type. `None` when nothing is said.
pub fn display(v: &Value, data_type: Option<&str>, scope: Scope) -> Result<Option<Value>, String> {
    let m = match v {
        Value::Null => return Ok(None),
        Value::String(s) => {
            let mut m = Map::new();
            m.insert("type".into(), Value::from(s.clone()));
            m
        }
        Value::Object(m) => m.clone(),
        other => return Err(format!("a display_type is a name or a mapping with a type, not {other}")),
    };
    let kind = match m.get("type") {
        Some(Value::String(t)) if !t.trim().is_empty() => t.trim().to_lowercase(),
        _ => return Err("a display_type needs a type".into()),
    };
    let kind = match kind.as_str() {
        "url" => "link".to_string(),
        "phone_number" => "phone".to_string(),
        "default" => return Ok(None),
        _ => kind,
    };
    if kind.starts_with("x-") {
        let mut out = m;
        out.insert("type".into(), Value::from(kind));
        return Ok(Some(Value::Object(out)));
    }
    let Some((_, takes)) = DISPLAY_TYPES.iter().find(|(k, _)| *k == kind) else {
        let known: Vec<&str> = DISPLAY_TYPES.iter().map(|(k, _)| *k).collect();
        return Err(format!(
            "display_type '{kind}' is not one this engine knows ({}); a client's own type starts with x-",
            known.join(", ")
        ));
    };
    if let (Some(takes), Some(dt)) = (takes, data_type) {
        if !takes.contains(&dt) {
            return Err(format!("display_type {kind} renders text, and this column is {dt}"));
        }
    }
    let allowed = display_options(&kind);
    let mut out = Map::new();
    out.insert("type".into(), Value::from(kind.clone()));
    for (k, v) in m {
        if k == "type" || v.is_null() {
            continue;
        }
        if !allowed.contains(&k.as_str()) {
            return Err(if allowed.is_empty() {
                format!("display_type {kind} takes no options, and '{k}' is one")
            } else {
                format!("display_type {kind} takes {}, not '{k}'", allowed.join(", "))
            });
        }
        let Value::String(t) = &v else { return Err(format!("display_type {kind}: {k} is a template, not {v}")) };
        out.insert(k, Value::from(template(t, scope)?));
    }
    Ok(Some(Value::Object(out)))
}

/// The names a template's slots read, as written and in order.
pub fn slots(t: &str) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    let mut rest = t;
    while let Some(at) = rest.find("{{") {
        let after = &rest[at + 2..];
        let Some(end) = after.find("}}") else { return Err(format!("template '{t}' opens a slot with {{{{ and never closes it")) };
        let name = after[..end].trim();
        if name.is_empty() {
            return Err(format!("template '{t}' has an empty slot; write {{{{value}}}}"));
        }
        names.push(name.to_string());
        rest = &after[end + 2..];
    }
    Ok(names)
}

/// A template checked against what its scope lets it read, with every
/// slot spelled `{{name}}` as the column it reads is named, so a client
/// looks the name up in the row as it is.
fn template(t: &str, scope: Scope) -> Result<String, String> {
    let mut out = String::with_capacity(t.len());
    let mut rest = t;
    slots(t)?;
    while let Some(at) = rest.find("{{") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let end = after.find("}}").expect("checked by slots");
        let name = after[..end].trim();
        let spelled = if name.eq_ignore_ascii_case(VALUE) {
            VALUE.to_string()
        } else {
            match scope {
                Scope::Field => {
                    return Err(format!(
                        "template '{t}' reads {{{{{name}}}}}; a field's template reads only {{{{value}}}}, and a query's projection may read its other columns"
                    ))
                }
                Scope::Query(columns) => match columns.iter().find(|c| c.trim().eq_ignore_ascii_case(name)) {
                    Some(c) => c.clone(),
                    None => {
                        return Err(format!(
                            "template '{t}' reads {{{{{name}}}}}, which is not a column of this query; it has {}",
                            columns.iter().map(|c| format!("'{c}'")).collect::<Vec<_>>().join(", ")
                        ))
                    }
                },
            }
        };
        out.push_str("{{");
        out.push_str(&spelled);
        out.push_str("}}");
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The slots a format or display type reads, besides `{{value}}`: the
/// columns a result must carry for it to render.
pub fn reads(v: &Value) -> Vec<String> {
    let mut names = Vec::new();
    if let Value::Object(m) = v {
        let kind = m.get("type").and_then(Value::as_str).unwrap_or("");
        if kind.starts_with("x-") {
            return names;
        }
        for (k, t) in m {
            if k == "type" {
                continue;
            }
            if let Value::String(t) = t {
                for n in slots(t).unwrap_or_default() {
                    if !n.eq_ignore_ascii_case(VALUE) && !names.contains(&n) {
                        names.push(n);
                    }
                }
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shortcuts_read_as_before() {
        assert_eq!(format_shortcut("currency:2").unwrap(), Some(json!({"type": "currency", "precision": 2})));
        assert_eq!(format_shortcut("number:2:abbreviate").unwrap(), Some(json!({"type": "number", "precision": 2, "abbreviate": true})));
        assert_eq!(format_shortcut("date:short").unwrap(), Some(json!({"type": "date", "pattern": "%b %d, %Y"})));
        assert_eq!(format_shortcut("datetime:%H:%M").unwrap(), Some(json!({"type": "datetime", "pattern": "%H:%M"})));
        assert_eq!(format_shortcut("raw").unwrap(), Some(json!({"type": "raw"})));
        assert_eq!(
            format_shortcut("template:https://x.io/{{value}}").unwrap(),
            Some(json!({"type": "template", "template": "https://x.io/{{value}}"}))
        );
        assert!(format_shortcut("nope").unwrap_err().contains("not a format"));
        assert!(format_shortcut("html:<b>{{value}}</b>").unwrap_err().contains("display_type: html"));
        assert!(format_shortcut("javascript:x => x").unwrap_err().contains("not supported"));
        assert!(format_shortcut("number:many").unwrap_err().contains("precision"));
    }

    #[test]
    fn a_mapping_is_checked_and_kept_in_one_shape() {
        let f = format(&json!({"type": "Number", "precision": "2", "abbreviate": "yes", "delimiter": ""}), Scope::Field).unwrap();
        assert_eq!(f, Some(json!({"type": "number", "precision": 2, "abbreviate": true})));
        assert_eq!(
            format(&json!({"type": "percent", "abbreviate": false}), Scope::Field).unwrap_err(),
            "format percent takes precision, not 'abbreviate'"
        );
        assert!(format(&json!({"precision": 2}), Scope::Field).unwrap_err().contains("needs a type"));
        assert!(format(&json!({"type": "number", "precision": 30}), Scope::Field).is_err());
        assert!(format(&json!({"type": "template"}), Scope::Field).unwrap_err().contains("template text"));
        assert_eq!(format(&Value::Null, Scope::Field).unwrap(), None);
    }

    #[test]
    fn a_fields_template_reads_only_its_value() {
        let ok = format(&json!({"type": "template", "template": "https://crm/{{ Value }}"}), Scope::Field).unwrap();
        assert_eq!(ok.unwrap()["template"], "https://crm/{{value}}");
        let e = format(&json!({"type": "template", "template": "{{Account Id}}"}), Scope::Field).unwrap_err();
        assert!(e.contains("reads only {{value}}"), "{e}");
        assert!(slots("a {{value").unwrap_err().contains("never closes"));
        assert!(slots("a {{ }}").unwrap_err().contains("empty slot"));
    }

    #[test]
    fn a_projections_template_reads_the_querys_columns() {
        let cols = vec!["Account Name".to_string(), "Account Id".to_string()];
        let d = display(
            &json!({"type": "link", "href": "https://crm/{{account id}}", "label": "{{value}}"}),
            Some("string"),
            Scope::Query(&cols),
        )
        .unwrap()
        .unwrap();
        assert_eq!(d, json!({"type": "link", "href": "https://crm/{{Account Id}}", "label": "{{value}}"}));
        assert_eq!(reads(&d), vec!["Account Id".to_string()]);
        let e = display(&json!({"type": "link", "href": "{{Region}}"}), None, Scope::Query(&cols)).unwrap_err();
        assert!(e.contains("not a column of this query") && e.contains("'Account Id'"), "{e}");
    }

    #[test]
    fn display_types() {
        assert_eq!(display(&json!("url"), Some("integer"), Scope::Field).unwrap(), Some(json!({"type": "link"})));
        assert_eq!(display(&json!("phone_number"), None, Scope::Field).unwrap(), Some(json!({"type": "phone"})));
        assert_eq!(display(&json!("default"), None, Scope::Field).unwrap(), None);
        assert_eq!(display(&json!("none"), None, Scope::Field).unwrap(), Some(json!({"type": "none"})));
        assert!(display(&json!("transcript"), Some("integer"), Scope::Field).unwrap_err().contains("renders text"));
        assert!(display(&json!("sparkle"), None, Scope::Field).unwrap_err().contains("x-"));
        assert!(display(&json!({"type": "email", "href": "x"}), None, Scope::Field).unwrap_err().contains("no options"));
        let own = json!({"type": "x-chart", "anything": {"goes": 1}});
        assert_eq!(display(&own, Some("integer"), Scope::Field).unwrap(), Some(own));
    }
}

//! Rewrites inside user SQL fragments: qualifying column references with a
//! table alias, and substituting `[Entity]@m` references.

use regex::Regex;
use zsql_dialect::Dialect;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

pub use zsql_spec::expr::{literal_pattern, quote_pair};

/// Rails `ExpressionParser.qualify`: every column reference in `sql` gets the
/// table alias. Each column is applied once and the pass is idempotent, so a
/// column repeated in the list is harmless.
pub fn qualify(sql: &str, columns: &[String], dialect: &Dialect, table_alias: &str) -> String {
    let mut seen: Vec<&str> = Vec::new();
    let mut acc = sql.to_string();
    for col in columns {
        if seen.contains(&col.as_str()) {
            continue;
        }
        seen.push(col);
        acc = column_gsub(&acc, col, dialect, table_alias);
    }
    acc
}

/// Rails `ExpressionParser.column_gsub`: qualifies one column reference with
/// `table_alias`, walking the SQL token by token so that
/// - a quoted identifier is one opaque unit, matched whole and case-sensitively,
///   and its interior is never touched (`"Enrollment (K-12)"` stays intact);
/// - a bare word matches case-insensitively, but never when it is a function
///   call (`date(...)` beside a `Date` column) or already qualified (`T0.date`);
/// - string literals pass through untouched.
pub fn column_gsub(sql: &str, col: &str, dialect: &Dialect, table_alias: &str) -> String {
    let (open, close) = quote_pair(dialect.get("quote").and_then(|v| v.as_str()).unwrap_or(""));
    let qualified = format!("{}.{}", table_alias, dialect.quote(col).unwrap_or_else(|_| format!("\"{col}\"")));
    let re = token_regex(&open, &close);
    let mut out = String::with_capacity(sql.len());
    let mut last = 0;
    for caps in re.captures_iter(sql) {
        let m = caps.get(0).unwrap();
        out.push_str(&sql[last..m.start()]);
        last = m.end();
        let after_dot = sql[..m.start()].ends_with('.');
        let replacement = if after_dot {
            None
        } else if let Some(ident) = caps.name("ident") {
            (ident.as_str() == col).then_some(&qualified)
        } else if let Some(word) = caps.name("word") {
            (caps.name("call").is_none() && word.as_str().eq_ignore_ascii_case(col)).then_some(&qualified)
        } else {
            None
        };
        out.push_str(replacement.map(String::as_str).unwrap_or(m.as_str()));
    }
    out.push_str(&sql[last..]);
    out
}

/// One token at a time: a string literal, a quoted identifier, or a bare word
/// (with a trailing paren captured when it is a call). One regex per quote
/// pair, compiled once: the same few pairs serve every expression.
fn token_regex(open: &str, close: &str) -> Arc<Regex> {
    static CACHE: LazyLock<Mutex<HashMap<(String, String), Arc<Regex>>>> = LazyLock::new(Default::default);
    let key = (open.to_string(), close.to_string());
    if let Some(re) = CACHE.lock().unwrap().get(&key) {
        return re.clone();
    }
    let (o, c) = (regex::escape(open), regex::escape(close));
    let pattern = format!(
        r"{lit}|{o}(?P<ident>(?:[^{c}]|{c}{c})+){c}|(?P<word>\b[a-zA-Z_][a-zA-Z0-9_]*\b)(?P<call>\s*\()?",
        lit = literal_pattern(open)
    );
    let re = Arc::new(Regex::new(&pattern).expect("token regex"));
    CACHE.lock().unwrap().insert(key, re.clone());
    re
}

/// Rails `ExpressionParser.entity_gsub`: `[Name]`, `[Name]@m`, `[Name]@d`
/// (case-insensitive, inner whitespace tolerated) become the replacement.
/// A hand-rolled scan: this runs once per reference of every calculation
/// rendered, and compiling a regex per call dominated SQL generation.
pub fn entity_gsub(sql: &str, entity_name: &str, replacement: &str) -> String {
    let wanted = entity_name.trim();
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    while let Some(open) = rest.find('[') {
        let Some(close_rel) = rest[open + 1..].find(']') else { break };
        let close = open + 1 + close_rel;
        let inner = rest[open + 1..close].trim();
        if inner.eq_ignore_ascii_case(wanted) || inner.to_lowercase() == wanted.to_lowercase() {
            out.push_str(&rest[..open]);
            out.push_str(replacement);
            let mut after = close + 1;
            // An optional `@m`/`@d` suffix belongs to the reference.
            let tail = rest[after..].as_bytes();
            if tail.len() >= 2 && tail[0] == b'@' && matches!(tail[1], b'm' | b'd' | b'M' | b'D') {
                after += 2;
            }
            rest = &rest[after..];
        } else {
            out.push_str(&rest[..close + 1]);
            rest = &rest[close + 1..];
        }
    }
    out.push_str(rest);
    out
}

pub use zsql_spec::expr::{has_aggregates, parse_refs, ungroupable};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualifies_bare_and_quoted() {
        let d = zsql_dialect::Dialects::builtin().get("postgres").unwrap();
        assert_eq!(column_gsub("sum(ws_net_paid)", "ws_net_paid", &d, "T0"), "sum(T0.\"ws_net_paid\")");
        assert_eq!(column_gsub("\"limit\" + 1", "limit", &d, "T1"), "T1.\"limit\" + 1");
        assert_eq!(column_gsub("ws_net_paid_inc_tax", "ws_net_paid", &d, "T0"), "ws_net_paid_inc_tax");
    }

    #[test]
    fn quoted_identifiers_are_opaque_and_calls_are_not_columns() {
        let d = zsql_dialect::Dialects::builtin().get("postgres").unwrap();
        assert_eq!(column_gsub("\"Enrollment (K-12)\"", "Enrollment (K-12)", &d, "T0"), "T0.\"Enrollment (K-12)\"");
        assert_eq!(column_gsub("\"Enrollment (K-12)\"", "K", &d, "T0"), "\"Enrollment (K-12)\"");
        assert_eq!(column_gsub("date(substr(\"Date\", 1, 4) || '-01')", "Date", &d, "T0"), "date(substr(T0.\"Date\", 1, 4) || '-01')");
        assert_eq!(column_gsub("T0.date + 1", "date", &d, "T0"), "T0.date + 1");
        assert_eq!(column_gsub("'date' || date", "date", &d, "T0"), "'date' || T0.\"date\"");
        let twice = qualify("\"date\" || \"date\"", &["date".to_string(), "date".to_string()], &d, "T0");
        assert_eq!(twice, "T0.\"date\" || T0.\"date\"");
        assert_eq!(qualify(&twice, &["date".to_string()], &d, "T0"), twice);
    }

    #[test]
    fn entity_references_are_replaced_whole() {
        assert_eq!(entity_gsub("sum([Net Paid]@m) / [ net paid ]", "net paid", "X"), "sum(X) / X");
        assert_eq!(entity_gsub("[Other]@d + [Net Paid]", "Net Paid", "X"), "[Other]@d + X");
        assert_eq!(entity_gsub("no refs here", "Net Paid", "X"), "no refs here");
        assert_eq!(entity_gsub("[Net Paid]@M@d", "Net Paid", "X"), "X@d");
    }

    #[test]
    fn backtick_dialect_treats_double_quotes_as_literals() {
        let d = zsql_dialect::Dialects::builtin().get("mysql").unwrap();
        assert_eq!(column_gsub("`date` || \"date\"", "date", &d, "T0"), "T0.`date` || \"date\"");
    }

}

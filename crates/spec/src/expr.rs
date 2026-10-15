//! Formula parsing shared by spec lowering and the planner: bracket
//! references (`[Name]@m`, `[Name]@d`, `[Alias]`) and aggregate detection.
//! Mirrors the parts of Rails' `Sql::ExpressionParser` that classify a
//! calculation and find what it reads.

use regex::Regex;
use std::sync::LazyLock;

/// Rails `ExpressionParser#has_aggregate_functions?`.
static FUNCTION_CALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b([a-z_][a-z0-9_]*)\(").unwrap());
static BRACKET_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\](@[mdMD])?").unwrap());
static WINDOW_CLAUSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bover\s*\(").unwrap());

/// The formula with string literals and ANSI-quoted identifiers replaced
/// and every bracket reference removed, lower-cased: what Rails' parser
/// scans for functions, so `[Running Sum(Revenue)]` is never a call.
fn formula_body(sql: &str) -> String {
    let scrubbed = scrub(&sql.to_lowercase(), "\"", "\"", &mut |_| {});
    BRACKET_REF.replace_all(&scrubbed, "").into_owned()
}

pub fn has_aggregates(sql: &str, aggregates: &[&str]) -> bool {
    let body = formula_body(sql);
    FUNCTION_CALL.captures_iter(&body).any(|c| aggregates.contains(&&c[1]))
}

/// Bracket references in a formula: (`@m` names, `@d` names, untyped aliases).
pub fn parse_refs(sql: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    let stripped = scrub(sql, "\"", "\"", &mut |_| {});
    let (mut m, mut d, mut p) = (Vec::new(), Vec::new(), Vec::new());
    for c in BRACKET_REF.captures_iter(&stripped) {
        let name = c[1].trim().to_string();
        match c.get(2).map(|t| t.as_str().to_lowercase()) {
            Some(t) if t == "@m" => {
                if !m.contains(&name) {
                    m.push(name);
                }
            }
            Some(_) => {
                if !d.contains(&name) {
                    d.push(name);
                }
            }
            None => {
                if !p.contains(&name) {
                    p.push(name);
                }
            }
        }
    }
    (m, d, p)
}

/// Rails `ExpressionParser.ungroupable?`: aggregates or windows cannot be grouped by.
pub fn ungroupable(sql: &str, aggregates: &[&str]) -> bool {
    let body = formula_body(sql);
    if WINDOW_CLAUSE.is_match(&body) {
        return true;
    }
    FUNCTION_CALL.captures_iter(&body).any(|c| aggregates.contains(&&c[1]))
}

// ------------------------------------------------------------ full parser

/// The opening and closing identifier quote characters an adapter's quote
/// template (`"@exp"`, `` `@exp` ``, `[@exp]`) wraps around a name. Bracket
/// quoting is Strata's field-reference syntax, so it falls back to ANSI
/// double quotes.
pub fn quote_pair(template: &str) -> (String, String) {
    let template = if template.is_empty() || template.starts_with('[') { "\"@exp\"" } else { template };
    let (open, close) = template.split_once("@exp").unwrap_or((template, ""));
    let close = if close.is_empty() { open } else { close };
    (open.to_string(), close.to_string())
}

/// String literals: single-quoted always, double-quoted too on a warehouse
/// whose identifier quote is something else (MySQL, BigQuery).
pub fn literal_pattern(open: &str) -> &'static str {
    if open == "\"" {
        r#"'(?:[^']|'')*'"#
    } else {
        r#"'(?:[^']|'')*'|"(?:[^"]|"")*""#
    }
}

/// Rails `Sql::ExpressionParser`: what a field expression or calculation
/// formula reads. Columns are the bare words and quoted identifiers that
/// are neither reserved keywords nor function names; bracket references
/// are `[Name]@m` measures, `[Name]@d` dimensions and untyped `[Alias]`
/// projection references.
#[derive(Debug, Clone, Default)]
pub struct ParsedExpression {
    /// Table columns the expression reads; quoted identifiers keep their
    /// case, bare words are lower-cased (Rails lower-cases the formula).
    pub columns: Vec<String>,
    pub field_refs: Vec<String>,
    pub measure_refs: Vec<String>,
    pub dimension_refs: Vec<String>,
    pub functions: Vec<String>,
    pub windowed: bool,
    /// `count(*)` and friends.
    pub wild: bool,
    aggregates: Vec<String>,
    scrubbed: String,
}

impl ParsedExpression {
    /// `reserved` and `aggregates` are the dialect's lists (lower-case);
    /// `quote` is its identifier quote template.
    pub fn parse(expression: &str, reserved: &[String], aggregates: &[String], quote: &str) -> Self {
        let (open, close) = quote_pair(quote);
        let mut quoted: Vec<String> = Vec::new();
        let sanitized = scrub(expression, &open, &close, &mut |ident| {
            if !quoted.contains(&ident) {
                quoted.push(ident);
            }
        })
        .to_lowercase();

        let mut out = ParsedExpression { aggregates: aggregates.to_vec(), ..Default::default() };
        // Bracketed references first, so a parenthesis inside a reference
        // name cannot read as a call.
        let mut matched: Vec<String> = Vec::new();
        for c in BRACKET_REF.captures_iter(&sanitized) {
            let name = c[1].trim().to_string();
            matched.push(c[0].to_string());
            match c.get(2).map(|m| m.as_str()) {
                Some("@m") => push_unique(&mut out.measure_refs, name),
                Some("@d") => push_unique(&mut out.dimension_refs, name),
                _ => push_unique(&mut out.field_refs, name),
            }
        }
        let mut sanitized = sanitized;
        for m in &matched {
            sanitized = sanitized.replace(m.as_str(), "");
        }
        for c in FUNCTION_CALL.captures_iter(&sanitized) {
            out.functions.push(c[1].to_string());
        }
        out.windowed = WINDOW_CLAUSE.is_match(&sanitized);
        out.columns.extend(quoted);
        static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b([a-zA-Z_][a-zA-Z0-9_]*)\b").unwrap());
        for c in WORD.captures_iter(&sanitized) {
            let w = c[1].to_string();
            if !reserved.contains(&w) && !out.functions.contains(&w) && !out.columns.contains(&w) {
                out.columns.push(w);
            }
        }
        static WILD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\w+\(\s?\*\s?\)").unwrap());
        out.wild = WILD.is_match(&sanitized);
        out.scrubbed = scrub(&expression.to_lowercase(), &open, &close, &mut |_| {});
        out
    }

    pub fn has_aggregate_functions(&self) -> bool {
        self.functions.iter().any(|f| self.aggregates.contains(f))
    }

    pub fn has_window_functions(&self) -> bool {
        self.windowed
    }

    /// The `@d` references that sit outside every aggregate call: what SQL
    /// demands be grouped when the expression aggregates.
    pub fn unaggregated_dimension_refs(&self) -> Vec<String> {
        static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]@d|\[[^\]]+\](?:@m)?|([a-z_][a-z0-9_]*)?\(|\)").unwrap());
        let mut refs: Vec<String> = Vec::new();
        let mut stack: Vec<bool> = Vec::new();
        for c in TOKEN.captures_iter(&self.scrubbed) {
            let token = &c[0];
            if let Some(d) = c.get(1) {
                let r = d.as_str().trim().to_string();
                if stack.is_empty() && !refs.contains(&r) {
                    refs.push(r);
                }
            } else if token.starts_with('[') {
                continue;
            } else if token == ")" {
                stack.pop();
            } else {
                let f = c.get(2).map(|m| m.as_str()).unwrap_or("");
                stack.push(self.aggregates.iter().any(|a| a == f));
            }
        }
        refs
    }
}

fn push_unique(v: &mut Vec<String>, s: String) {
    if !v.contains(&s) {
        v.push(s);
    }
}

/// Replaces string literals with a placeholder literal and quoted
/// identifiers with `0`, yielding each identifier's name (case intact,
/// doubled quote characters unescaped), so what remains reads as bare
/// references, keywords and functions.
fn scrub(expr: &str, open: &str, close: &str, on_ident: &mut dyn FnMut(String)) -> String {
    let re = scrub_regex(open, close);
    re.replace_all(expr, |c: &regex::Captures| {
        if let Some(ident) = c.get(1) {
            on_ident(ident.as_str().to_string());
            "0".to_string()
        } else {
            "'__str__'".to_string()
        }
    })
    .into_owned()
}

fn scrub_regex(open: &str, close: &str) -> std::sync::Arc<Regex> {
    static CACHE: LazyLock<std::sync::Mutex<std::collections::HashMap<(String, String), std::sync::Arc<Regex>>>> =
        LazyLock::new(Default::default);
    let key = (open.to_string(), close.to_string());
    if let Some(re) = CACHE.lock().unwrap().get(&key) {
        return re.clone();
    }
    let (o, c) = (regex::escape(open), regex::escape(close));
    let pattern = format!(r"{lit}|{o}((?:[^{c}]|{c}{c})+){c}", lit = literal_pattern(open));
    let re = std::sync::Arc::new(Regex::new(&pattern).expect("scrub regex"));
    CACHE.lock().unwrap().insert(key, re.clone());
    re
}

#[cfg(test)]
mod parser_tests {
    use super::*;

    fn parse(sql: &str) -> ParsedExpression {
        let reserved: Vec<String> = [
            "__str__",
            "as",
            "cast",
            "case",
            "when",
            "then",
            "else",
            "end",
            "and",
            "or",
            "not",
            "null",
            "from",
            "in",
            "is",
            "over",
            "partition",
            "by",
            "order",
            "day",
            "month",
            "year",
            "date",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let aggregates: Vec<String> = ["sum", "count", "avg", "min", "max"].iter().map(|s| s.to_string()).collect();
        ParsedExpression::parse(sql, &reserved, &aggregates, "\"@exp\"")
    }

    #[test]
    fn columns_functions_and_references() {
        let p = parse("sum(ss_net_paid) / count(*)");
        assert_eq!(p.columns, vec!["ss_net_paid"]);
        assert_eq!(p.functions, vec!["sum", "count"]);
        assert!(p.has_aggregate_functions());
        assert!(p.wild);

        let p = parse("\"Enrollment (K-12)\" || ' - ' || district");
        assert_eq!(p.columns, vec!["Enrollment (K-12)", "district"]);

        let p = parse("sum([Net Paid]@m) / [Rows] + [Region]@d");
        assert_eq!(p.measure_refs, vec!["net paid"]);
        assert_eq!(p.dimension_refs, vec!["region"]);
        assert_eq!(p.field_refs, vec!["rows"]);
        assert!(p.columns.is_empty());

        let p = parse("date(substr(\"Date\", 1, 4) || '-01')");
        assert_eq!(p.columns, vec!["Date"]);
        assert_eq!(p.functions, vec!["date", "substr"]);
    }

    #[test]
    fn unaggregated_dimension_refs_ignore_those_inside_aggregates() {
        let p = parse("case when [Region]@d is null then null else sum(case when [Category]@d = 'x' then 1 end) end");
        assert_eq!(p.unaggregated_dimension_refs(), vec!["region"]);
        assert!(parse("sum(x) over (partition by y)").has_window_functions());
    }
}

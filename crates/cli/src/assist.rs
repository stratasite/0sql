//! The repl's line helper: tab completion, the dimmed hint as you type,
//! color, and a line that continues while a parenthesis is open. The
//! grammar says what may come next (`zsql_shorthand::complete`); this
//! maps each offer to the model's fields, sampled values and tables.

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Context, Helper};
use serde_json::Value;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use zsql_shorthand::complete::{context, functions, parts, types, Kinds, Offer};

/// What the helper needs from the model: its fields and tables once, and
/// a dimension's values on demand.
pub trait Lookup {
    fn catalog(&mut self) -> Catalog;
    fn values(&mut self, dim: &str) -> Vec<String>;
}

#[derive(Debug, Default, Clone)]
pub struct Catalog {
    pub dimensions: Vec<String>,
    pub measures: Vec<String>,
    pub tables: Vec<String>,
}

impl Catalog {
    /// From the `fields` and `tables` answers.
    pub fn from_answers(fields: &Value, tables: &Value) -> Catalog {
        let mut c = Catalog::default();
        for f in fields["fields"].as_array().into_iter().flatten() {
            let Some(name) = f["name"].as_str() else { continue };
            if f["kind"] == "measure" {
                c.measures.push(name.to_string());
            } else {
                c.dimensions.push(name.to_string());
            }
        }
        for t in tables["tables"].as_array().into_iter().flatten() {
            if let Some(uid) = t["uid"].as_str() {
                c.tables.push(uid.to_string());
            }
        }
        c
    }
}

/// The sampled values of a `values` answer, as text.
pub fn values_of(answer: &Value) -> Vec<String> {
    answer["values"].as_array().into_iter().flatten().filter_map(|x| x["v"].as_str().or_else(|| x.as_str()).map(str::to_string)).collect()
}

impl<S: crate::side::Side> Lookup for Rc<RefCell<S>> {
    fn catalog(&mut self) -> Catalog {
        let mut side = self.borrow_mut();
        let fields = side.fields("", false).unwrap_or(Value::Null);
        let tables = side.tables().unwrap_or(Value::Null);
        Catalog::from_answers(&fields, &tables)
    }
    fn values(&mut self, dim: &str) -> Vec<String> {
        self.borrow_mut().values(dim).map(|v| values_of(&v)).unwrap_or_default()
    }
}

pub struct Assist<L: Lookup> {
    lookup: RefCell<L>,
    catalog: RefCell<Catalog>,
    values: RefCell<HashMap<String, Vec<String>>>,
}

impl<L: Lookup> Assist<L> {
    pub fn new(mut lookup: L) -> Self {
        let catalog = lookup.catalog();
        Assist { lookup: RefCell::new(lookup), catalog: RefCell::new(catalog), values: RefCell::new(HashMap::new()) }
    }

    /// After a reload or a branch switch: read the model again.
    pub fn refresh(&self) {
        let catalog = self.lookup.borrow_mut().catalog();
        *self.catalog.borrow_mut() = catalog;
        self.values.borrow_mut().clear();
    }

    fn values_for(&self, dim: &str) -> Vec<String> {
        let key = dim.to_lowercase();
        if let Some(v) = self.values.borrow().get(&key) {
            return v.clone();
        }
        let v = self.lookup.borrow_mut().values(dim);
        self.values.borrow_mut().insert(key, v.clone());
        v
    }

    /// Where the replacement starts, and each candidate. The start is the
    /// longest prefix among the offers; a candidate for a shorter prefix
    /// carries the text between the two so it replaces the same span.
    pub fn candidates(&self, line: &str, pos: usize) -> (usize, Vec<Pair>) {
        let pos = pos.min(line.len());
        let offers = context(line, pos);
        let longest = offers.iter().map(|o| o.prefix().len()).max().unwrap_or(0);
        let start = pos - longest;
        let mut out: Vec<Pair> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let catalog = self.catalog.borrow();
        for offer in &offers {
            let prefix = offer.prefix();
            let keep = &line[start..pos - prefix.len()];
            let candidates: Vec<String> = match offer {
                Offer::Fields { kinds, .. } => {
                    let pool: Vec<&String> = match kinds {
                        Kinds::Any => catalog.dimensions.iter().chain(catalog.measures.iter()).collect(),
                        Kinds::Dimension => catalog.dimensions.iter().collect(),
                        Kinds::Measure => catalog.measures.iter().collect(),
                    };
                    matching(prefix, pool.into_iter().map(String::as_str), true)
                }
                Offer::Functions { .. } => {
                    matching(prefix, functions().iter().map(String::as_str), false).into_iter().map(|f| format!("{f}(")).collect()
                }
                Offer::Keywords { words, .. } => {
                    // A half-typed keyword after a name: only when the name
                    // before it is a field (or a closed function call).
                    let before = keep.trim();
                    let lower = before.to_lowercase();
                    let bare = lower.trim_end_matches("@m").trim_end_matches("@d").trim_matches(|c| c == '[' || c == '"');
                    let named = before.is_empty()
                        || before.ends_with(')')
                        || catalog.dimensions.iter().chain(catalog.measures.iter()).any(|f| f.to_lowercase() == bare);
                    if !prefix.is_empty() && !named {
                        continue;
                    }
                    matching(prefix, words.iter().copied(), false)
                }
                Offer::Values { field, .. } => {
                    let values = self.values_for(field);
                    matching(prefix, values.iter().map(String::as_str), true).into_iter().map(|v| quoted_if_needed(&v)).collect()
                }
                Offer::Projected { names, .. } => {
                    if names.is_empty() {
                        matching(prefix, catalog.dimensions.iter().map(String::as_str), true)
                    } else {
                        matching(prefix, names.iter().map(String::as_str), true)
                    }
                }
                Offer::Tables { .. } => matching(prefix, catalog.tables.iter().map(String::as_str), true),
                Offer::Parts { .. } => matching(prefix, parts().into_iter(), false),
                Offer::Types { .. } => matching(prefix, types().into_iter(), false),
            };
            for c in candidates {
                let replacement = format!("{keep}{c}");
                if seen.insert(replacement.to_lowercase()) {
                    out.push(Pair { display: c, replacement });
                }
            }
        }
        (start, out)
    }

    /// The text a right arrow would add: what every candidate agrees on
    /// beyond what is typed, when something is typed, or the single
    /// candidate when nothing is.
    pub fn hint_for(&self, line: &str, pos: usize) -> Option<String> {
        if pos != line.len() {
            return None;
        }
        let (start, cands) = self.candidates(line, pos);
        let typed = &line[start..pos];
        let suffixes: Vec<&str> = cands
            .iter()
            .filter_map(|c| {
                let r = c.replacement.as_str();
                (r.len() >= typed.len() && r[..typed.len()].eq_ignore_ascii_case(typed)).then(|| &r[typed.len()..])
            })
            .collect();
        if suffixes.is_empty() {
            return None;
        }
        if typed.trim().is_empty() {
            return (suffixes.len() == 1).then(|| suffixes[0].to_string());
        }
        let mut common = suffixes[0];
        for s in &suffixes[1..] {
            let n = common.chars().zip(s.chars()).take_while(|(a, b)| a.eq_ignore_ascii_case(b)).map(|(a, _)| a.len_utf8()).sum();
            common = &common[..n];
        }
        (!common.is_empty()).then(|| common.to_string())
    }
}

/// The candidates that begin with the typed text, in their own order;
/// for names and values, when none does, those that contain it.
fn matching<'a>(prefix: &str, pool: impl Iterator<Item = &'a str>, within: bool) -> Vec<String> {
    let want = prefix.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let pool: Vec<&str> = pool.collect();
    // What is typed in full is not a completion of itself.
    let starts: Vec<String> =
        pool.iter().filter(|c| c.to_lowercase().starts_with(&want) && c.to_lowercase() != want).map(|c| c.to_string()).collect();
    if !starts.is_empty() || want.is_empty() || !within {
        return starts;
    }
    pool.iter().filter(|c| c.to_lowercase().contains(&want) && c.to_lowercase() != want).map(|c| c.to_string()).collect()
}

/// A value the grammar would read wrongly bare is single-quoted.
fn quoted_if_needed(v: &str) -> String {
    let words: Vec<&str> = v.split_whitespace().collect();
    let needs = v.is_empty()
        || v.contains(|c: char| ",()[]{}'\"".contains(c))
        || words.iter().any(|w| w.eq_ignore_ascii_case("and") || w.eq_ignore_ascii_case("or"))
        || v != v.trim();
    if needs {
        format!("'{}'", v.replace('\'', "''"))
    } else {
        v.to_string()
    }
}

// ------------------------------------------------------------- rustyline

impl<L: Lookup> Completer for Assist<L> {
    type Candidate = Pair;
    fn complete(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> rustyline::Result<(usize, Vec<Pair>)> {
        if line.trim_start().starts_with('.') {
            return Ok((pos, vec![]));
        }
        Ok(self.candidates(line, pos))
    }
}

impl<L: Lookup> Hinter for Assist<L> {
    type Hint = String;
    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        if line.trim_start().starts_with('.') || line.is_empty() {
            return None;
        }
        self.hint_for(line, pos)
    }
}

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

/// The first opener (a parenthesis, bracket, brace or quote) that nothing
/// closes, or none. A line is unfinished while it has one.
pub fn unclosed(line: &str) -> Option<usize> {
    let mut stack: Vec<(usize, char)> = Vec::new();
    let mut chars = line.char_indices().peekable();
    let mut prev: Option<char> = None;
    while let Some((i, c)) = chars.next() {
        let at_token_start = prev.is_none_or(|p| p.is_whitespace() || ",()[]{}@=<>!".contains(p));
        prev = Some(c);
        match stack.last() {
            Some((_, q)) if matches!(q, '\'' | '"' | ']') => {
                let q = *q;
                if c == q {
                    if q == '\'' && chars.peek().is_some_and(|(_, n)| *n == '\'') {
                        chars.next();
                        continue;
                    }
                    stack.pop();
                }
            }
            _ => match c {
                '\'' if at_token_start => stack.push((i, '\'')),
                '"' => stack.push((i, '"')),
                '[' => stack.push((i, ']')),
                '(' => stack.push((i, ')')),
                '{' => stack.push((i, '}')),
                ')' | '}' if stack.last().is_some_and(|(_, q)| *q == c) => {
                    stack.pop();
                }
                _ => {}
            },
        }
    }
    stack.first().map(|(i, _)| *i)
}

const RESERVED: [&str; 14] =
    ["and", "or", "not", "in", "is", "between", "top", "contains", "starts", "ends", "keyword", "as", "asc", "desc"];
const CONTEXTUAL: [&str; 17] = [
    "by", "with", "for", "left", "inner", "dedupe", "neq", "lt", "none", "null", "limit", "hints", "include", "exclude", "expand", "order",
    "pct",
];

/// Keywords bold, strings dim, an unclosed opener and what follows it red.
pub fn color(line: &str) -> String {
    let red_from = unclosed(line);
    let mut out = String::with_capacity(line.len() * 2);
    let mut i = 0;
    let mut depth = 0usize;
    while i < line.len() {
        if red_from == Some(i) {
            out.push_str(RED);
            out.push_str(&line[i..]);
            out.push_str(RESET);
            return out;
        }
        let rest = &line[i..];
        let c = rest.chars().next().unwrap();
        let at_token_start = line[..i].chars().next_back().is_none_or(|p| p.is_whitespace() || ",()[]{}@=<>!".contains(p));
        if c == '\'' && at_token_start {
            let end = rest[1..].find('\'').map(|e| i + e + 2).unwrap_or(line.len());
            out.push_str(DIM);
            out.push_str(&line[i..end]);
            out.push_str(RESET);
            i = end;
            continue;
        }
        if c == '"' || c == '[' {
            let closer = if c == '"' { '"' } else { ']' };
            let end = rest[1..].find(closer).map(|e| i + e + 2).unwrap_or(line.len());
            out.push_str(&line[i..end]);
            i = end;
            continue;
        }
        if c == '(' {
            depth += 1;
        }
        if c == ')' {
            depth = depth.saturating_sub(1);
        }
        if c.is_alphabetic() || c == '_' {
            let len = rest.find(|ch: char| !(ch.is_alphanumeric() || ch == '_')).unwrap_or(rest.len());
            let word = &rest[..len];
            let lower = word.to_lowercase();
            let after = rest[len..].trim_start();
            let is_function = after.starts_with('(')
                && (zsql_shorthand::complete::functions().contains(&lower) || matches!(lower.as_str(), "calc" | "extract" | "custom"));
            let bold = RESERVED.contains(&lower.as_str())
                || is_function
                || (lower == "order" && after.to_lowercase().starts_with("by"))
                || (lower == "by" && depth > 0)
                || (CONTEXTUAL.contains(&lower.as_str()) && lower != "order" && lower != "by" && depth == 0);
            if bold {
                out.push_str(BOLD);
                out.push_str(word);
                out.push_str(RESET);
            } else {
                out.push_str(word);
            }
            i += len;
            continue;
        }
        out.push(c);
        i += c.len_utf8();
    }
    out
}

impl<L: Lookup> Highlighter for Assist<L> {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if line.trim_start().starts_with('.') {
            return Cow::Borrowed(line);
        }
        Cow::Owned(color(line))
    }
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(format!("{DIM}{hint}{RESET}"))
    }
    fn highlight_char(&self, _line: &str, _pos: usize, kind: CmdKind) -> bool {
        !matches!(kind, CmdKind::ForcedRefresh)
    }
}

impl<L: Lookup> Validator for Assist<L> {
    fn validate(&self, ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        let input = ctx.input();
        if input.trim_start().starts_with('.') {
            return Ok(ValidationResult::Valid(None));
        }
        if unclosed(input).is_none() {
            return Ok(ValidationResult::Valid(None));
        }
        // A dot command on a continuation line cannot run until the line
        // is closed; say so rather than swallow it.
        if input.rsplit('\n').next().is_some_and(|last| last.trim_start().starts_with('.')) {
            return Ok(ValidationResult::Invalid(Some(
                "\n  an opener is still unclosed above; close it, or press Ctrl-C to clear the line".into(),
            )));
        }
        Ok(ValidationResult::Incomplete)
    }
}

impl<L: Lookup> Helper for Assist<L> {}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;
    impl Lookup for Fake {
        fn catalog(&mut self) -> Catalog {
            Catalog {
                dimensions: vec!["Country".into(), "Region".into(), "Order Date".into(), "Item Category".into(), "Customer Id".into()],
                measures: vec!["Store Net Paid".into(), "Catalog Net Paid".into(), "Orders".into()],
                tables: vec!["store-sales".into(), "store".into()],
            }
        }
        fn values(&mut self, dim: &str) -> Vec<String> {
            match dim.to_lowercase().as_str() {
                "country" => vec!["Canada".into(), "United States".into(), "Rock and Roll".into(), "O'Neil, Inc".into()],
                _ => vec![],
            }
        }
    }
    fn assist() -> Assist<Fake> {
        Assist::new(Fake)
    }
    fn names(line: &str) -> (usize, Vec<String>) {
        let (start, c) = assist().candidates(line, line.len());
        (start, c.into_iter().map(|p| p.replacement).collect())
    }

    #[test]
    fn fields_functions_and_keywords_by_position() {
        let (start, c) = names("country, sto");
        assert_eq!(start, 9);
        assert_eq!(c, ["Store Net Paid"]);
        let (_, c) = names("country, store n");
        assert_eq!(c, ["Store Net Paid"]);
        let (_, c) = names("mo");
        assert!(c.contains(&"month(".to_string()) && c.contains(&"moving_avg(".to_string()) && !c.iter().any(|x| x == "Orders"));
        let (_, c) = names("month(ord");
        assert_eq!(c, ["Order Date", "Orders"].iter().filter(|_| true).map(|s| s.to_string()).take(1).collect::<Vec<_>>());
        let (_, c) = names("yoy(ord");
        assert_eq!(c, ["Orders"]);
        let (start, c) = names("country ");
        assert_eq!(start, 0, "the name may continue, so the span is the whole item");
        assert!(c.iter().any(|x| x == "country in (") && c.iter().any(|x| x == "country desc"));
        assert!(!c.iter().any(|x| x.eq_ignore_ascii_case("country")), "a field typed in full is not offered again");
        let (_, c) = names("country i");
        assert_eq!(c, ["country in (", "country is null", "country is not null"]);
        let (_, c) = names("Store Net Paid d");
        assert_eq!(c, ["Store Net Paid desc"]);
        let (_, c) = names("month(d) d");
        assert_eq!(c, ["desc"], "after a closed call only the half-typed word is replaced");
        let (_, c) = names("category top 5 by ");
        assert_eq!(c, ["Store Net Paid", "Catalog Net Paid", "Orders"]);
        let (_, c) = names("hints st");
        assert_eq!(c, ["store-sales", "store"]);
        let (_, c) = names("extract(d");
        assert_eq!(c, ["day_of_month", "day_of_week", "day_of_year", "day_name"]);
        let (_, c) = names("calc(count(*), i");
        assert_eq!(c, ["integer"]);
        let (_, c) = names("region, share(x by r");
        assert_eq!(c, ["region"], "a partition offers the projected dimensions");
        let (_, c) = names("share(x by r");
        assert_eq!(c, ["Region"], "with nothing projected yet, any dimension");
    }

    #[test]
    fn values_are_quoted_when_the_grammar_needs_it() {
        let (start, c) = names("country = ");
        assert_eq!(start, 10);
        assert_eq!(c, ["Canada", "United States", "'Rock and Roll'", "'O''Neil, Inc'"]);
        let (_, c) = names("country in (Canada, u");
        assert_eq!(c, ["United States"]);
        let (_, c) = names("region = ");
        assert!(c.is_empty(), "no sampled values, nothing offered");
        let (_, c) = names("country = 'Can");
        assert!(c.is_empty());
    }

    #[test]
    fn hints_complete_what_is_unambiguous() {
        let a = assist();
        assert_eq!(a.hint_for("country, sto", 12).as_deref(), Some("re Net Paid"));
        assert_eq!(a.hint_for("country, cat", 12).as_deref(), Some("alog Net Paid"));
        assert_eq!(a.hint_for("country = Can", 13).as_deref(), Some("ada"));
        assert_eq!(a.hint_for("country, or", 11).as_deref(), Some("der"), "Order Date and Orders agree on `der`");
        assert_eq!(a.hint_for("x between 1 ", 12).as_deref(), Some("and "));
        assert_eq!(a.hint_for("x in", 4).as_deref(), Some(" ("));
        assert_eq!(a.hint_for("x is ", 5), None, "null or not null: two answers, no hint");
        assert_eq!(a.hint_for("country, sto", 5), None, "no hint away from the end");
        assert_eq!(a.hint_for("", 0), None);
    }

    #[test]
    fn colour_and_balance() {
        assert_eq!(color("country = Canada"), "country = Canada");
        assert_eq!(color("x desc"), format!("x {BOLD}desc{RESET}"));
        assert_eq!(color("order date, month(d)"), format!("order date, {BOLD}month{RESET}(d)"));
        assert_eq!(
            color("rank(x by a order by b)"),
            format!("{BOLD}rank{RESET}(x {BOLD}by{RESET} a {BOLD}order{RESET} {BOLD}by{RESET} b)")
        );
        assert_eq!(color("created by"), "created by");
        assert_eq!(color("x = 'a, b'"), format!("x = {DIM}'a, b'{RESET}"));
        assert_eq!(color("x = 'open"), format!("x = {RED}'open{RESET}"));
        assert_eq!(color("month(x"), format!("{BOLD}month{RESET}{RED}(x{RESET}"));
        assert_eq!(unclosed("a (b [c] 'd') e"), None);
        assert_eq!(unclosed("a (b"), Some(2));
        assert_eq!(unclosed("who = O'Brien"), None, "an apostrophe inside a word is a letter");
        assert_eq!(unclosed("'it''s'"), None);
        assert_eq!(unclosed("x = 'a"), Some(4));
        assert_eq!(color("who = O'Brien"), "who = O'Brien");
    }
}

//! Where the cursor stands in a line, for a line editor's completion and
//! hints: the construct it is inside and what the grammar can take there.
//! Nothing here resolves a name; the editor maps each offer to the
//! model's fields, values and tables. The scan is lexical and tolerant,
//! so a half-typed line never fails, it only offers less.

use crate::parse::{decorator_family, PARTS, RANK_FUNCTIONS, TEMPORALS, TRUNCATES, TYPES};

/// Which fields an offer wants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kinds {
    Any,
    Dimension,
    Measure,
}

/// What may be written at the cursor, each with the text already typed
/// towards it (its prefix), so the editor can replace that text.
#[derive(Debug, Clone, PartialEq)]
pub enum Offer {
    Fields {
        prefix: String,
        kinds: Kinds,
    },
    /// Decorator functions, calc, extract and custom; the editor appends `(`.
    Functions {
        prefix: String,
    },
    /// Fixed words, each a whole token or phrase: `in (`, `is not null`, `as`.
    Keywords {
        prefix: String,
        words: Vec<&'static str>,
    },
    /// A dimension's sampled values.
    Values {
        prefix: String,
        field: String,
    },
    /// A projected dimension, for `by` and `order by`; empty when the
    /// line's projections could not be read, in which case any dimension.
    Projected {
        prefix: String,
        names: Vec<String>,
    },
    Tables {
        prefix: String,
    },
    Parts {
        prefix: String,
    },
    Types {
        prefix: String,
    },
}

impl Offer {
    pub fn prefix(&self) -> &str {
        match self {
            Offer::Fields { prefix, .. }
            | Offer::Functions { prefix }
            | Offer::Keywords { prefix, .. }
            | Offer::Values { prefix, .. }
            | Offer::Projected { prefix, .. }
            | Offer::Tables { prefix }
            | Offer::Parts { prefix }
            | Offer::Types { prefix } => prefix,
        }
    }
}

pub const PREDICATES: [&str; 15] = [
    "= ",
    "!= ",
    "> ",
    ">= ",
    "< ",
    "<= ",
    "in (",
    "not in (",
    "between ",
    "contains ",
    "starts with ",
    "ends with ",
    "is null",
    "is not null",
    "top ",
];
const MODIFIERS: [&str; 3] = ["as ", "asc", "desc"];
const ITEM_WORDS: [&str; 5] = ["include (", "exclude (", "expand (", "limit ", "via "];

/// Every function name the grammar reads before a `(`.
pub fn functions() -> Vec<String> {
    let mut out: Vec<String> = vec!["calc".into(), "extract".into(), "custom".into()];
    out.extend(TRUNCATES.iter().map(|s| s.to_string()));
    for t in TEMPORALS {
        out.push(t.to_string());
        out.push(format!("{t}_pct"));
    }
    out.push("share".into());
    out.push("share_filtered".into());
    for f in ["sum", "avg", "min", "max", "count"] {
        out.push(format!("running_{f}"));
        out.push(format!("moving_{f}"));
    }
    out.extend(RANK_FUNCTIONS.iter().map(|s| s.to_string()));
    out
}

pub fn parts() -> Vec<&'static str> {
    PARTS.to_vec()
}

pub fn types() -> Vec<&'static str> {
    TYPES.to_vec()
}

#[derive(Debug, Clone, PartialEq)]
enum Frame {
    Top,
    /// A segment's `(...)`: items separated by commas or newlines.
    Group {
        expand: bool,
    },
    /// A function's `(...)`.
    Args {
        func: String,
    },
    /// `in (...)` or `keyword (...)`: values.
    List,
    /// `calc(...)`: SQL, then a type after a top-level comma.
    Sql {
        comma: Option<usize>,
        depth: usize,
    },
    /// `with (...)` after `expand`.
    Carry,
    /// `for (...)` or `via (...)`.
    Names {
        via: bool,
    },
    /// A filter group's `(`.
    Paren,
    Json {
        depth: usize,
    },
    /// A string, a quoted name or a bracketed name, by its closer.
    Quote(char),
}

struct Open {
    frame: Frame,
    /// Just after the `(` (or the opener) that began it.
    start: usize,
    /// After the last separator inside it.
    seg: usize,
}

/// What the grammar can take at `pos`, with the typed prefix of each.
pub fn context(line: &str, pos: usize) -> Vec<Offer> {
    let pos = pos.min(line.len());
    let head = match line.get(..pos) {
        Some(h) => h,
        None => return vec![],
    };
    let mut stack: Vec<Open> = vec![Open { frame: Frame::Top, start: 0, seg: 0 }];
    let chars: Vec<(usize, char)> = head.char_indices().collect();
    let mut k = 0;
    while k < chars.len() {
        let (i, c) = chars[k];
        let top = stack.last_mut().unwrap();
        match &mut top.frame {
            Frame::Quote(q) => {
                if c == *q {
                    if *q == '\'' && chars.get(k + 1).is_some_and(|(_, n)| *n == '\'') {
                        k += 2;
                        continue;
                    }
                    stack.pop();
                }
            }
            Frame::Json { depth } => match c {
                '"' => {
                    k += 1;
                    while k < chars.len() && chars[k].1 != '"' {
                        if chars[k].1 == '\\' {
                            k += 1;
                        }
                        k += 1;
                    }
                }
                '{' => *depth += 1,
                '}' => {
                    *depth -= 1;
                    if *depth == 0 {
                        stack.pop();
                    }
                }
                _ => {}
            },
            Frame::Sql { comma, depth } => match c {
                '\'' => stack.push(Open { frame: Frame::Quote('\''), start: i + 1, seg: i + 1 }),
                '"' => stack.push(Open { frame: Frame::Quote('"'), start: i + 1, seg: i + 1 }),
                '[' => stack.push(Open { frame: Frame::Quote(']'), start: i + 1, seg: i + 1 }),
                '(' => *depth += 1,
                ')' if *depth > 0 => *depth -= 1,
                ')' => {
                    stack.pop();
                }
                ',' if *depth == 0 => *comma = Some(i + 1),
                _ => {}
            },
            _ => match c {
                '\'' => stack.push(Open { frame: Frame::Quote('\''), start: i + 1, seg: i + 1 }),
                '"' => stack.push(Open { frame: Frame::Quote('"'), start: i + 1, seg: i + 1 }),
                '[' => stack.push(Open { frame: Frame::Quote(']'), start: i + 1, seg: i + 1 }),
                '{' => stack.push(Open { frame: Frame::Json { depth: 1 }, start: i + 1, seg: i + 1 }),
                '(' => {
                    let word = word_before(head, i).to_ascii_lowercase();
                    let frame = match word.as_str() {
                        "calc" => Frame::Sql { comma: None, depth: 0 },
                        "in" | "keyword" => Frame::List,
                        "with" => Frame::Carry,
                        "for" => Frame::Names { via: false },
                        "via" => Frame::Names { via: true },
                        "include" | "exclude" => Frame::Group { expand: false },
                        "expand" => Frame::Group { expand: true },
                        "extract" | "custom" => Frame::Args { func: word.clone() },
                        w if decorator_family(w).is_some() => Frame::Args { func: word.clone() },
                        _ => Frame::Paren,
                    };
                    stack.push(Open { frame, start: i + 1, seg: i + 1 });
                }
                ')' => {
                    if stack.len() > 1 {
                        stack.pop();
                    }
                }
                ',' if !matches!(top.frame, Frame::Args { .. }) => top.seg = i + 1,
                '\n' | '\r' if matches!(top.frame, Frame::Top | Frame::Group { .. }) => top.seg = i + 1,
                _ => {}
            },
        }
        k += 1;
    }

    let top = stack.last().unwrap();
    let text = head[top.seg..].trim_start();
    let item_start = stack.iter().rev().find(|o| matches!(o.frame, Frame::Top | Frame::Group { .. })).map(|o| o.seg).unwrap_or(0);
    match &top.frame {
        Frame::Quote(_) | Frame::Json { .. } => vec![],
        Frame::Sql { comma: None, .. } => vec![],
        Frame::Sql { comma: Some(at), .. } => vec![Offer::Types { prefix: head[*at..].trim_start().to_string() }],
        Frame::List => {
            // The field is what stands before `in (` or `keyword (` in the
            // item around this list.
            let enclosing = stack.iter().rev().nth(1).map(|o| o.seg).unwrap_or(0);
            let before = head[enclosing..top.start - 1].trim();
            let lower = before.to_ascii_lowercase();
            let field = ["not in", "in", "keyword"]
                .iter()
                .find_map(|kw| lower.strip_suffix(kw).map(|f| f.trim_end()))
                .map(|f| before[..f.len()].to_string())
                .unwrap_or_default();
            vec![Offer::Values { prefix: text.to_string(), field: strip_kind(&field) }]
        }
        Frame::Carry => {
            if words_of(text).iter().any(|w| w.eq_ignore_ascii_case("as")) {
                vec![]
            } else {
                vec![Offer::Fields { prefix: text.to_string(), kinds: Kinds::Dimension }]
            }
        }
        Frame::Names { via: true } => vec![Offer::Tables { prefix: text.to_string() }],
        Frame::Names { via: false } => vec![Offer::Fields { prefix: text.to_string(), kinds: Kinds::Measure }],
        Frame::Args { func } => args(func, text, &projected(line, item_start)),
        Frame::Group { .. } => item(text, Mode::Group),
        Frame::Paren => item(text, Mode::Filter),
        Frame::Top => item(text, Mode::Top),
    }
}

/// The word that ends just before `at`, over spaces.
fn word_before(s: &str, at: usize) -> &str {
    let before = s[..at].trim_end();
    let start = before.rfind(|c: char| c.is_whitespace() || ",()[]{}@=<>!\"'".contains(c)).map(|i| i + 1).unwrap_or(0);
    &before[start..]
}

fn words_of(s: &str) -> Vec<&str> {
    s.split_whitespace().collect()
}

fn strip_kind(field: &str) -> String {
    let f = field.trim();
    let lower = f.to_ascii_lowercase();
    let f = if lower.ends_with("@m") || lower.ends_with("@d") { &f[..f.len() - 2] } else { f };
    f.trim_matches(|c| c == '[' || c == ']' || c == '"').to_string()
}

/// The dimensions the line projects before this item, by the name written.
fn projected(line: &str, item_start: usize) -> Vec<String> {
    let before = &line[..item_start];
    let Ok(items) = crate::parse::parse(before) else { return vec![] };
    let mut names = Vec::new();
    for it in items {
        if let crate::parse::ItemKind::Projection(p) = it.kind {
            let mut e = &p.expr;
            loop {
                match e {
                    crate::parse::Expr::Ref(f) => {
                        if f.kind != Some(crate::parse::Kind::Measure) {
                            names.push(f.name.clone());
                        }
                        break;
                    }
                    crate::parse::Expr::Extract { inner, .. }
                    | crate::parse::Expr::Custom { inner, .. }
                    | crate::parse::Expr::Deco { inner, .. } => e = inner,
                    crate::parse::Expr::Calc { .. } => break,
                }
            }
        }
    }
    names
}

/// Inside a decorator's parentheses.
fn args(func: &str, text: &str, projected: &[String]) -> Vec<Offer> {
    let family = decorator_family(func).unwrap_or(func);
    let kinds = match family {
        "truncate" | "extract" | "custom" => Kinds::Dimension,
        _ => Kinds::Measure,
    };
    match func {
        "extract" => {
            return match text.split_once(',') {
                None => vec![Offer::Parts { prefix: text.to_string() }],
                Some((_, after)) => inner(after.trim_start(), kinds),
            };
        }
        "custom" => {
            return match text.split_once(',') {
                None => vec![],
                Some((_, after)) => inner(after.trim_start(), kinds),
            };
        }
        _ => {}
    }
    // The partition and order clauses, if begun: the last `order by` or
    // `by` as a whole word decides which.
    let words: Vec<(usize, &str)> = text.char_indices().filter(|(_, c)| !c.is_whitespace()).fold(Vec::new(), |mut acc, (i, _)| {
        if acc.last().is_none_or(|(s, w): &(usize, &str)| *s + w.len() < i) {
            let w = text[i..].split_whitespace().next().unwrap_or("");
            if acc.last().is_none_or(|(s, pw)| i >= *s + pw.len()) {
                acc.push((i, w));
            }
        }
        acc
    });
    let mut clause: Option<(bool, usize)> = None; // (order, start of the refs)
    let mut idx = 0;
    while idx < words.len() {
        let (i, w) = words[idx];
        if w.eq_ignore_ascii_case("order") && words.get(idx + 1).is_some_and(|(_, n)| n.eq_ignore_ascii_case("by")) {
            let (bi, bw) = words[idx + 1];
            clause = Some((true, bi + bw.len()));
            idx += 2;
            continue;
        }
        if w.eq_ignore_ascii_case("by") && i > 0 {
            clause = Some((false, i + w.len()));
        }
        idx += 1;
    }
    let ends_in_space = text.ends_with(char::is_whitespace);
    match clause {
        Some((order, from)) => {
            let refs = &text[from..];
            let current = refs.rsplit(',').next().unwrap_or("").trim_start();
            let mut offers = vec![Offer::Projected { prefix: current.to_string(), names: projected.to_vec() }];
            if ends_in_space && !current.trim().is_empty() {
                let words = if order { vec!["asc", "desc"] } else { vec!["order by "] };
                offers.push(Offer::Keywords { prefix: String::new(), words });
            }
            offers
        }
        None => {
            if let Some((_, after)) = text.split_once(',') {
                // A number: nothing to offer until it is written.
                return if ends_in_space && after.trim().parse::<i64>().is_ok() {
                    vec![Offer::Keywords { prefix: String::new(), words: vec!["by ", "order by "] }]
                } else {
                    vec![]
                };
            }
            let mut offers = inner(text, kinds);
            if ends_in_space && !text.trim().is_empty() && family != "truncate" {
                offers.push(Offer::Keywords { prefix: String::new(), words: vec!["by ", "order by "] });
            }
            offers
        }
    }
}

/// A decorator's inner field: a name or a nested function.
fn inner(text: &str, kinds: Kinds) -> Vec<Offer> {
    let mut offers = vec![Offer::Fields { prefix: text.to_string(), kinds }];
    if !text.contains(char::is_whitespace) {
        offers.push(Offer::Functions { prefix: text.to_string() });
    }
    offers
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Top,
    Group,
    Filter,
}

/// One item, as far as it has been typed.
fn item(text: &str, mode: Mode) -> Vec<Offer> {
    let lower = text.to_ascii_lowercase();
    let words = words_of(text);
    let ends_in_space = text.ends_with(char::is_whitespace);
    if text.is_empty() {
        let mut offers = vec![Offer::Fields { prefix: String::new(), kinds: Kinds::Any }, Offer::Functions { prefix: String::new() }];
        if mode == Mode::Top {
            offers.push(Offer::Keywords { prefix: String::new(), words: ITEM_WORDS.to_vec() });
        }
        return offers;
    }
    if text.starts_with('{') {
        return vec![];
    }
    let first = words[0].to_ascii_lowercase();
    if mode == Mode::Top && (words.len() > 1 || ends_in_space) {
        match first.as_str() {
            "limit" => return vec![],
            "via" => return vec![Offer::Tables { prefix: text[words[0].len()..].trim_start().to_string() }],
            "include" | "exclude" | "expand" => return segment_tail(text, first == "expand"),
            _ => {}
        }
    }
    // A function call: `month(x)` then modifiers only.
    if let (Some(open), Some(close)) = (lower.find('('), lower.rfind(')')) {
        let name = lower[..open].trim();
        if open < close
            && !name.contains(char::is_whitespace)
            && (decorator_family(name).is_some() || matches!(name, "calc" | "extract" | "custom"))
        {
            return after_expr(&text[close + 1..], mode);
        }
    }
    // The field, up to a predicate word or an operator.
    let mut field_end = text.len();
    let mut pred: Option<(usize, &str)> = None;
    for (i, w) in word_positions(text) {
        let wl = w.to_ascii_lowercase();
        let op = ["!=", "<>", ">=", "<=", "=", ">", "<"].iter().find(|o| wl.starts_with(*o)).copied();
        if let Some(op) = op {
            pred = Some((i, op));
            field_end = i;
            break;
        }
        if matches!(
            wl.as_str(),
            "in" | "not" | "between" | "contains" | "starts" | "ends" | "is" | "top" | "keyword" | "as" | "asc" | "desc" | "and" | "or"
        ) && i > 0
        {
            pred = Some((i, &text[i..i + w.len()]));
            field_end = i;
            break;
        }
    }
    let field = strip_kind(text[..field_end].trim());
    let Some((at, word)) = pred else {
        // Only the field so far: continue it, or follow it.
        let mut offers = vec![Offer::Fields { prefix: text.to_string(), kinds: Kinds::Any }];
        if !text.contains(char::is_whitespace) {
            offers.push(Offer::Functions { prefix: text.to_string() });
            if mode == Mode::Top {
                offers.push(Offer::Keywords { prefix: text.to_string(), words: ITEM_WORDS.to_vec() });
            }
        }
        // After a complete field, a predicate or a modifier: whole, or
        // half-typed as the last word.
        if ends_in_space || words.len() >= 2 {
            let mut kws: Vec<&'static str> = PREDICATES.to_vec();
            if mode == Mode::Top {
                kws.extend(MODIFIERS);
            }
            let prefix = if ends_in_space { String::new() } else { words.last().unwrap().to_string() };
            offers.push(Offer::Keywords { prefix, words: kws });
        }
        return offers;
    };
    let rest = text[at + word.len()..].trim_start();
    let rest_words = words_of(rest);
    let kw = word.to_ascii_lowercase();
    // `and` or `or` after the value begins another predicate; `between`
    // owns the first `and`, and a list's own text is skipped.
    if !matches!(kw.as_str(), "and" | "or" | "as" | "asc" | "desc") {
        let scan = match kw.as_str() {
            "in" | "keyword" => rest.find(')').map(|c| &rest[c + 1..]).unwrap_or(""),
            _ => rest,
        };
        if let Some(after) = conjunction_after(scan, kw == "between") {
            return item(after.trim_start(), Mode::Filter);
        }
    }
    let value_then = |offers: &mut Vec<Offer>, value: &str| {
        if value.starts_with('\'') {
            return;
        }
        if ends_in_space && !value.trim().is_empty() {
            offers.push(Offer::Keywords { prefix: String::new(), words: vec!["and ", "or "] });
        } else {
            offers.push(Offer::Values { prefix: value.to_string(), field: field.clone() });
        }
    };
    let mut offers = Vec::new();
    match kw.as_str() {
        "and" | "or" => return item(rest, Mode::Filter),
        "as" => {
            if ends_in_space && !rest.trim().is_empty() {
                offers.push(Offer::Keywords { prefix: String::new(), words: vec!["asc", "desc"] });
            }
        }
        "asc" | "desc" => {}
        "not" => {
            if rest_words.is_empty() {
                offers.push(Offer::Keywords { prefix: String::new(), words: vec!["in (", "contains ", "starts with ", "ends with "] });
            } else {
                return item(&format!("{} {}", text[..field_end].trim(), rest), mode);
            }
        }
        "in" | "keyword" => {
            if rest.is_empty() {
                offers.push(Offer::Keywords { prefix: String::new(), words: vec![if ends_in_space { "(" } else { " (" }] });
            } else if rest.starts_with(')') && ends_in_space {
                offers.push(Offer::Keywords { prefix: String::new(), words: vec!["and ", "or "] });
            }
        }
        "between" => {
            let lower_rest = rest.to_ascii_lowercase();
            let and_at = lower_rest.find(" and ").or_else(|| lower_rest.strip_suffix(" and").map(|s| s.len()));
            match and_at {
                None => {
                    if ends_in_space && !rest.trim().is_empty() {
                        offers.push(Offer::Keywords { prefix: String::new(), words: vec!["and "] });
                    } else if !rest.starts_with('\'') {
                        offers.push(Offer::Values { prefix: rest.to_string(), field: field.clone() });
                    }
                }
                Some(i) => {
                    let upper = rest[i..].trim_start_matches(|c: char| c.is_whitespace() || c.is_alphabetic()).trim_start();
                    let upper = if rest[i..].trim_start().to_ascii_lowercase().starts_with("and") {
                        rest[i..].trim_start()[3..].trim_start()
                    } else {
                        upper
                    };
                    value_then(&mut offers, upper);
                }
            }
        }
        "contains" => value_then(&mut offers, rest),
        "starts" | "ends" => {
            if rest_words.first().is_none_or(|w| !w.eq_ignore_ascii_case("with")) {
                offers.push(Offer::Keywords { prefix: rest.to_string(), words: vec!["with "] });
            } else {
                value_then(&mut offers, rest[4..].trim_start());
            }
        }
        "is" => offers.push(Offer::Keywords { prefix: rest.to_string(), words: vec!["null", "not null"] }),
        "top" => {
            if rest_words.is_empty() {
            } else if rest_words.len() == 1 && ends_in_space {
                offers.push(Offer::Keywords { prefix: String::new(), words: vec!["by "] });
            } else if rest_words.len() >= 2 && rest_words[1].eq_ignore_ascii_case("by") {
                let after = rest[rest_words[0].len()..].trim_start()[2..].trim_start();
                offers.push(Offer::Fields { prefix: after.to_string(), kinds: Kinds::Measure });
            } else if rest_words.len() == 1 {
                offers.push(Offer::Keywords { prefix: rest_words[0].to_string(), words: vec!["by "] }.filter_numeric(rest_words[0]));
            }
        }
        _ => value_then(&mut offers, rest), // an operator
    }
    offers
}

impl Offer {
    /// `top 1` is a count, not a keyword prefix: keep the offer only if
    /// the typed word could begin one of its words.
    fn filter_numeric(self, typed: &str) -> Offer {
        if typed.chars().all(|c| c.is_ascii_digit()) {
            return Offer::Keywords { prefix: String::new(), words: vec![] };
        }
        self
    }
}

/// After a closed function call: an alias or a direction.
fn after_expr(tail: &str, mode: Mode) -> Vec<Offer> {
    if mode != Mode::Top {
        return vec![];
    }
    let ends_in_space = tail.ends_with(char::is_whitespace);
    let words = words_of(tail);
    match words.as_slice() {
        [] if ends_in_space => vec![Offer::Keywords { prefix: String::new(), words: MODIFIERS.to_vec() }],
        [] => vec![],
        [w] if !ends_in_space => vec![Offer::Keywords { prefix: w.to_string(), words: MODIFIERS.to_vec() }],
        _ if words[0].eq_ignore_ascii_case("as") && ends_in_space && words.len() > 1 => {
            vec![Offer::Keywords { prefix: String::new(), words: vec!["asc", "desc"] }]
        }
        _ => vec![],
    }
}

/// After a segment's group: its tail words.
fn segment_tail(text: &str, expand: bool) -> Vec<Offer> {
    let Some(close) = text.rfind(')') else { return vec![] };
    let tail = &text[close + 1..];
    let words = words_of(tail);
    let ends_in_space = tail.ends_with(char::is_whitespace);
    let mut tails: Vec<&'static str> = vec!["as ", "for "];
    if expand {
        tails = vec!["with (", "as ", "left", "inner", "dedupe "];
        if text.to_ascii_lowercase().contains("with") {
            tails.remove(0);
        }
    }
    let last = words.last().map(|w| w.to_ascii_lowercase());
    if let Some(l) = &last {
        if ends_in_space {
            return match l.as_str() {
                "dedupe" => vec![Offer::Keywords { prefix: String::new(), words: vec!["neq", "lt", "none"] }],
                "for" => vec![Offer::Fields { prefix: String::new(), kinds: Kinds::Measure }],
                "as" => vec![],
                _ => vec![Offer::Keywords { prefix: String::new(), words: tails }],
            };
        }
        let before = words.get(words.len().wrapping_sub(2)).map(|w| w.to_ascii_lowercase());
        return match before.as_deref() {
            Some("dedupe") => vec![Offer::Keywords { prefix: l.clone(), words: vec!["neq", "lt", "none"] }],
            Some("for") => vec![Offer::Fields { prefix: words.last().unwrap().to_string(), kinds: Kinds::Measure }],
            Some("as") => vec![],
            _ => vec![Offer::Keywords { prefix: words.last().unwrap().to_string(), words: tails }],
        };
    }
    if ends_in_space {
        vec![Offer::Keywords { prefix: String::new(), words: tails }]
    } else {
        vec![]
    }
}

/// The text after the first `and` or `or` standing as a word in `s`,
/// skipping the first `and` when it belongs to a `between`.
fn conjunction_after(s: &str, skip_first_and: bool) -> Option<&str> {
    let mut skipped = !skip_first_and;
    for (i, w) in word_positions(s) {
        if w.eq_ignore_ascii_case("and") {
            if !skipped {
                skipped = true;
                continue;
            }
            return Some(&s[i + 3..]);
        }
        if w.eq_ignore_ascii_case("or") {
            return Some(&s[i + 2..]);
        }
    }
    None
}

/// Each whitespace-separated word with its byte offset.
fn word_positions(s: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let skip = rest.len() - rest.trim_start().len();
        i += skip;
        if i >= s.len() {
            break;
        }
        let rest = &s[i..];
        let len = rest.find(char::is_whitespace).unwrap_or(rest.len());
        out.push((i, &rest[..len]));
        i += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: &str) -> Vec<Offer> {
        context(line, line.len())
    }
    fn fields(offers: &[Offer]) -> Option<(&str, Kinds)> {
        offers.iter().find_map(|o| match o {
            Offer::Fields { prefix, kinds } => Some((prefix.as_str(), *kinds)),
            _ => None,
        })
    }
    fn keywords(offers: &[Offer]) -> Option<(&str, &[&'static str])> {
        offers.iter().find_map(|o| match o {
            Offer::Keywords { prefix, words } => Some((prefix.as_str(), words.as_slice())),
            _ => None,
        })
    }

    #[test]
    fn the_start_of_an_item() {
        let o = at("");
        assert_eq!(fields(&o), Some(("", Kinds::Any)));
        assert!(keywords(&o).is_some_and(|(_, w)| w.contains(&"include (")));
        let o = at("country, ");
        assert_eq!(fields(&o), Some(("", Kinds::Any)));
        let o = at("country, sto");
        assert_eq!(fields(&o), Some(("sto", Kinds::Any)));
        assert!(o.iter().any(|x| matches!(x, Offer::Functions { prefix } if prefix == "sto")));
        let o = at("store net");
        assert_eq!(fields(&o), Some(("store net", Kinds::Any)));
        assert!(!o.iter().any(|x| matches!(x, Offer::Functions { .. })), "a multi-word prefix is never a function");
        assert_eq!(keywords(&o).map(|(p, _)| p), Some("net"), "the last word may be a keyword beginning");
    }

    #[test]
    fn after_a_field_the_predicates_and_modifiers() {
        let o = at("country ");
        assert_eq!(fields(&o), Some(("country ", Kinds::Any)), "the name may continue");
        let (p, words) = keywords(&o).unwrap();
        assert_eq!(p, "");
        assert!(words.contains(&"in (") && words.contains(&"as ") && words.contains(&"desc"));
        let o = context("x, country ", 11);
        assert!(keywords(&o).is_some());
        let o = at("include (country ");
        let (_, words) = keywords(&o).unwrap();
        assert!(words.contains(&"= ") && !words.contains(&"as "), "no modifiers inside a segment");
    }

    #[test]
    fn values_after_a_predicate() {
        assert_eq!(at("country = Can"), vec![Offer::Values { prefix: "Can".into(), field: "country".into() }]);
        assert_eq!(at("country in (Can"), vec![Offer::Values { prefix: "Can".into(), field: "country".into() }]);
        assert_eq!(at("country not in (A, B"), vec![Offer::Values { prefix: "B".into(), field: "country".into() }]);
        assert_eq!(at("[net paid]@m > 1"), vec![Offer::Values { prefix: "1".into(), field: "net paid".into() }]);
        assert_eq!(at("country = 'Can"), vec![], "inside a string nothing is offered");
        assert_eq!(keywords(&at("country = Canada ")).map(|(_, w)| w.to_vec()), Some(vec!["and ", "or "]));
        assert_eq!(
            at("country = Canada and reg"),
            at("reg").into_iter().filter(|o| !matches!(o, Offer::Keywords { .. })).collect::<Vec<_>>()
        );
        assert_eq!(at("x in (a) and reg")[0], Offer::Fields { prefix: "reg".into(), kinds: Kinds::Any });
        assert_eq!(at("x between 1 and 2 or reg")[0], Offer::Fields { prefix: "reg".into(), kinds: Kinds::Any });
        assert_eq!(fields(&at("x = 1 and ")), Some(("", Kinds::Any)));
        assert_eq!(at("date between 1"), vec![Offer::Values { prefix: "1".into(), field: "date".into() }]);
        assert_eq!(keywords(&at("date between 1 ")).map(|(_, w)| w.to_vec()), Some(vec!["and "]));
        assert_eq!(at("date between 1 and 2"), vec![Offer::Values { prefix: "2".into(), field: "date".into() }]);
        assert_eq!(at("name contains ma"), vec![Offer::Values { prefix: "ma".into(), field: "name".into() }]);
        assert_eq!(keywords(&at("name starts ")).map(|(p, w)| (p.to_string(), w.to_vec())), Some((String::new(), vec!["with "])));
        assert_eq!(at("name starts with x"), vec![Offer::Values { prefix: "x".into(), field: "name".into() }]);
        assert_eq!(keywords(&at("x is n")).map(|(p, w)| (p.to_string(), w.to_vec())), Some(("n".to_string(), vec!["null", "not null"])));
        assert_eq!(keywords(&at("x not ")).map(|(_, w)| w.to_vec()), Some(vec!["in (", "contains ", "starts with ", "ends with "]));
        assert_eq!(at("x not in (a"), vec![Offer::Values { prefix: "a".into(), field: "x".into() }]);
        assert_eq!(keywords(&at("x in")).map(|(_, w)| w.to_vec()), Some(vec![" ("]));
        assert_eq!(keywords(&at("x in ")).map(|(_, w)| w.to_vec()), Some(vec!["("]));
    }

    #[test]
    fn top_n_and_segments() {
        assert_eq!(at("category top "), vec![]);
        assert_eq!(keywords(&at("category top 5 ")).map(|(_, w)| w.to_vec()), Some(vec!["by "]));
        assert_eq!(at("category top 5 by sto"), vec![Offer::Fields { prefix: "sto".into(), kinds: Kinds::Measure }]);
        assert!(keywords(&at("include (a) ")).is_some_and(|(_, w)| w == ["as ", "for "]));
        assert!(keywords(&at("expand (a) ")).is_some_and(|(_, w)| w.contains(&"with (")));
        assert!(keywords(&at("expand (a) with (b) ")).is_some_and(|(_, w)| !w.contains(&"with (")));
        assert_eq!(keywords(&at("expand (a) with (b) dedupe ")).map(|(_, w)| w.to_vec()), Some(vec!["neq", "lt", "none"]));
        assert_eq!(at("include (a) for sto"), vec![Offer::Fields { prefix: "sto".into(), kinds: Kinds::Measure }]);
        assert_eq!(at("include (a) for (m1, sto"), vec![Offer::Fields { prefix: "sto".into(), kinds: Kinds::Measure }]);
        assert_eq!(at("include (a) as Na"), vec![]);
        assert_eq!(at("expand (a) with (ite"), vec![Offer::Fields { prefix: "ite".into(), kinds: Kinds::Dimension }]);
        assert_eq!(at("include (a, b > 1\n  c"), at("c").into_iter().filter(|o| !matches!(o, Offer::Keywords { .. })).collect::<Vec<_>>());
    }

    #[test]
    fn inside_functions() {
        assert_eq!(fields(&at("month(ord")), Some(("ord", Kinds::Dimension)));
        assert_eq!(fields(&at("yoy(sto")), Some(("sto", Kinds::Measure)));
        assert_eq!(fields(&at("share(yoy(sto")), Some(("sto", Kinds::Measure)));
        assert!(at("extract(mo").contains(&Offer::Parts { prefix: "mo".into() }));
        assert_eq!(fields(&at("extract(month, ord")), Some(("ord", Kinds::Dimension)));
        assert_eq!(at("custom('up"), vec![]);
        assert_eq!(fields(&at("custom('upper(@expression)', cou")), Some(("cou", Kinds::Dimension)));
        assert_eq!(at("calc(sum([a"), vec![]);
        assert_eq!(at("calc(count(*), int"), vec![Offer::Types { prefix: "int".into() }]);
        let o = at("region, share(x ");
        assert_eq!(keywords(&o).map(|(_, w)| w.to_vec()), Some(vec!["by ", "order by "]));
        let o = at("region, share(x by re");
        assert_eq!(o[0], Offer::Projected { prefix: "re".into(), names: vec!["region".into()] });
        let o = at("region, month(d), rank(x by region order by d");
        assert_eq!(o[0], Offer::Projected { prefix: "d".into(), names: vec!["region".into(), "d".into()] });
        assert_eq!(keywords(&at("region, rank(x by region order by region ")).map(|(_, w)| w.to_vec()), Some(vec!["asc", "desc"]));
        assert_eq!(keywords(&at("region, rank(x by region ")).map(|(_, w)| w.to_vec()), Some(vec!["order by "]));
        assert_eq!(at("moving_avg(x, "), vec![]);
        assert_eq!(keywords(&at("moving_avg(x, 7 ")).map(|(_, w)| w.to_vec()), Some(vec!["by ", "order by "]));
        assert_eq!(keywords(&at("month(d) ")).map(|(_, w)| w.to_vec()), Some(vec!["as ", "asc", "desc"]));
        assert_eq!(keywords(&at("month(d) d")).map(|(p, _)| p.to_string()), Some("d".into()));
        assert_eq!(keywords(&at("month(d) as Mo ")).map(|(_, w)| w.to_vec()), Some(vec!["asc", "desc"]));
        assert_eq!(at("month(d) as "), vec![]);
    }

    #[test]
    fn opaque_places_offer_nothing() {
        assert_eq!(at("{\"name\": \"x"), vec![]);
        assert_eq!(at("[Net Pa"), vec![]);
        assert_eq!(at("\"Net Pa"), vec![]);
        assert_eq!(at("limit 1"), vec![]);
        assert_eq!(at("x as Al"), vec![]);
        assert_eq!(at("x desc "), vec![]);
        assert_eq!(at("via sto"), vec![Offer::Tables { prefix: "sto".into() }]);
        assert_eq!(at("via (a, sto"), vec![Offer::Tables { prefix: "sto".into() }]);
    }

    #[test]
    fn a_cursor_in_the_middle_reads_only_what_is_before_it() {
        let line = "country = Canada, region";
        assert_eq!(context(line, 12), vec![Offer::Values { prefix: "Ca".into(), field: "country".into() }]);
        assert_eq!(context(line, 7), fields(&at("country")).map(|_| at("country")).unwrap());
        assert_eq!(context(line, 1000), at(line));
    }
}

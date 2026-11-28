//! The formula language: what a formula column and a filter are
//! written in. Data prep's own, small, and typed, and compiled here to
//! the engine's SQL; it does not reuse `calc(...)` from the query
//! shorthand, which carries raw SQL.
//!
//! ```text
//! formula  := or
//! or       := and ("or" and)*
//! and      := not ("and" not)*
//! not      := "not" not | test
//! test     := sum (cmp sum | "is" ["not"] "null" | ["not"] "in" "(" list ")")?
//! cmp      := "=" | "!=" | "<>" | "<" | "<=" | ">" | ">="
//! sum      := product (("+" | "-" | "&") product)*
//! product  := unary (("*" | "/" | "%") unary)*
//! unary    := "-" unary | atom
//! atom     := number | string | "true" | "false" | "null"
//!           | column | function "(" list? ")" | "(" formula ")"
//! column   := name | "[" any name "]"
//! string   := '...' | "..."            a quote doubled is the quote
//! ```
//!
//! `&` joins text. A column is a bare name, or any name in brackets:
//! `[order date]`. Functions are only those on the allowlist below,
//! each with the types it takes and the type it gives, so a formula's
//! type is known before it runs and every result is cast to it. Names
//! are case-insensitive, as they are to the engine. `today()` and
//! `now()` are the time the run was pinned to, the same for every row
//! and every step of one run.

use crate::error::{Error, Result};
use crate::schema::{common, ident, literal, Column, ColumnType, DECIMAL_SCALE_MAX};
use ColumnType::*;

/// A formula, compiled: its SQL, its type, and the columns it reads.
#[derive(Clone, Debug, PartialEq)]
pub struct Compiled {
    pub sql: String,
    pub ty: ColumnType,
    pub reads: Vec<String>,
}

/// What a formula is compiled against: the columns there at its step,
/// and the time the run is pinned to.
pub struct Scope<'a> {
    pub columns: &'a [Column],
    /// `YYYY-MM-DDTHH:MM:SS.mmmZ`.
    pub pinned_at: &'a str,
}

/// The longest formula accepted, and the deepest nesting.
const FORMULA_LIMIT: usize = 4096;
const DEPTH_LIMIT: usize = 64;

pub fn compile(text: &str, scope: &Scope<'_>) -> Result<Compiled> {
    if text.len() > FORMULA_LIMIT {
        return Err(Error::Invalid(format!("a formula is at most {FORMULA_LIMIT} characters")));
    }
    let tokens = lex(text)?;
    let mut p = Parser { tokens, at: 0, depth: 0 };
    let ast = p.formula()?;
    if p.at < p.tokens.len() {
        return Err(bad(text, &format!("'{}' is not expected here", p.tokens[p.at].text())));
    }
    let mut reads = Vec::new();
    let (sql, ty) = typed(&ast, scope, &mut reads).map_err(|e| match e {
        Error::Invalid(m) => Error::Invalid(format!("in formula `{text}`: {m}")),
        other => other,
    })?;
    reads.sort();
    reads.dedup();
    Ok(Compiled { sql, ty: ty.unwrap_or(Text), reads })
}

/// Just the columns a formula names, without compiling it: what the
/// column graph needs of a formula whose columns may not all be there.
pub fn names(text: &str) -> Result<Vec<String>> {
    let mut p = Parser { tokens: lex(text)?, at: 0, depth: 0 };
    let ast = p.formula().map_err(|e| bad(text, &e.to_string()))?;
    let mut out = Vec::new();
    collect_columns(&ast, &mut out);
    out.sort_by_key(|n| n.to_lowercase());
    out.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    Ok(out)
}

fn collect_columns(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::Column(n) => out.push(n.clone()),
        Expr::Unary(_, a) | Expr::IsNull(a, _) => collect_columns(a, out),
        Expr::Binary(_, a, b) => {
            collect_columns(a, out);
            collect_columns(b, out);
        }
        Expr::In(a, list, _) => {
            collect_columns(a, out);
            list.iter().for_each(|x| collect_columns(x, out));
        }
        Expr::Call(_, args) => args.iter().for_each(|x| collect_columns(x, out)),
        Expr::Number(_) | Expr::Text(_) | Expr::Bool(_) | Expr::Null => {}
    }
}

fn bad(text: &str, why: &str) -> Error {
    Error::Invalid(format!("in formula `{text}`: {why}"))
}

// ------------------------------------------------------------- tokens

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Number(String),
    Text(String),
    Word(String),
    Bracketed(String),
    Op(&'static str),
}

impl Tok {
    fn text(&self) -> String {
        match self {
            Tok::Number(s) | Tok::Word(s) => s.clone(),
            Tok::Text(s) => format!("'{s}'"),
            Tok::Bracketed(s) => format!("[{s}]"),
            Tok::Op(o) => o.to_string(),
        }
    }
}

// Two-character operators first: the lexer takes the first that matches.
const OPS: [&str; 17] = ["<=", ">=", "!=", "<>", "==", "=", "<", ">", "+", "-", "*", "/", "%", "&", "(", ")", ","];

fn lex(text: &str) -> Result<Vec<Tok>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
                i += 1;
                if i < chars.len() && (chars[i] == '+' || chars[i] == '-') {
                    i += 1;
                }
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let n: String = chars[start..i].iter().collect();
            if n.parse::<f64>().is_err() {
                return Err(bad(text, &format!("'{n}' is not a number")));
            }
            out.push(Tok::Number(n));
        } else if c == '\'' || c == '"' {
            let mut s = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err(bad(text, "a string is not closed")),
                    Some(&q) if q == c => {
                        if chars.get(i + 1) == Some(&c) {
                            s.push(c);
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    }
                    Some(&x) => {
                        s.push(x);
                        i += 1;
                    }
                }
            }
            out.push(Tok::Text(s));
        } else if c == '[' {
            let close = chars[i + 1..].iter().position(|x| *x == ']').ok_or_else(|| bad(text, "a [column] is not closed"))?;
            let name: String = chars[i + 1..i + 1 + close].iter().collect();
            if name.trim().is_empty() {
                return Err(bad(text, "[] names no column"));
            }
            out.push(Tok::Bracketed(name));
            i += close + 2;
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(Tok::Word(chars[start..i].iter().collect()));
        } else {
            let rest: String = chars[i..chars.len().min(i + 2)].iter().collect();
            let op = OPS.iter().find(|o| rest.starts_with(**o)).ok_or_else(|| bad(text, &format!("'{c}' is not part of a formula")))?;
            out.push(Tok::Op(if *op == "==" { "=" } else { op }));
            i += op.len();
        }
    }
    Ok(out)
}

// ------------------------------------------------------------- the tree

#[derive(Clone, Debug, PartialEq)]
enum Expr {
    Number(String),
    Text(String),
    Bool(bool),
    Null,
    Column(String),
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    IsNull(Box<Expr>, bool),
    In(Box<Expr>, Vec<Expr>, bool),
    Call(String, Vec<Expr>),
}

struct Parser {
    tokens: Vec<Tok>,
    at: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.at)
    }

    fn word(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(x)) if x.eq_ignore_ascii_case(w))
    }

    fn op(&self, o: &str) -> bool {
        matches!(self.peek(), Some(Tok::Op(x)) if *x == o)
    }

    fn expect(&mut self, o: &str) -> Result<()> {
        if self.op(o) {
            self.at += 1;
            Ok(())
        } else {
            let found = self.peek().map(Tok::text).unwrap_or_else(|| "the end".into());
            Err(Error::Invalid(format!("'{o}' is expected where {found} is")))
        }
    }

    fn deeper(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > DEPTH_LIMIT {
            return Err(Error::Invalid(format!("a formula nests at most {DEPTH_LIMIT} deep")));
        }
        Ok(())
    }

    fn formula(&mut self) -> Result<Expr> {
        self.deeper()?;
        let mut e = self.and()?;
        while self.word("or") {
            self.at += 1;
            e = Expr::Binary("or", Box::new(e), Box::new(self.and()?));
        }
        self.depth -= 1;
        Ok(e)
    }

    fn and(&mut self) -> Result<Expr> {
        let mut e = self.not()?;
        while self.word("and") {
            self.at += 1;
            e = Expr::Binary("and", Box::new(e), Box::new(self.not()?));
        }
        Ok(e)
    }

    fn not(&mut self) -> Result<Expr> {
        if self.word("not") {
            self.at += 1;
            self.deeper()?;
            let e = Expr::Unary("not", Box::new(self.not()?));
            self.depth -= 1;
            return Ok(e);
        }
        self.test()
    }

    fn test(&mut self) -> Result<Expr> {
        let left = self.sum()?;
        for o in ["=", "!=", "<>", "<=", ">=", "<", ">"] {
            if self.op(o) {
                self.at += 1;
                let o = if o == "<>" { "!=" } else { o };
                let op: &'static str = ["=", "!=", "<=", ">=", "<", ">"].into_iter().find(|x| *x == o).unwrap_or("=");
                return Ok(Expr::Binary(op, Box::new(left), Box::new(self.sum()?)));
            }
        }
        if self.word("is") {
            self.at += 1;
            let negated = self.word("not");
            if negated {
                self.at += 1;
            }
            if !self.word("null") {
                return Err(Error::Invalid("'is' is followed by null or not null".into()));
            }
            self.at += 1;
            return Ok(Expr::IsNull(Box::new(left), negated));
        }
        let negated = self.word("not") && matches!(self.tokens.get(self.at + 1), Some(Tok::Word(w)) if w.eq_ignore_ascii_case("in"));
        if negated || self.word("in") {
            self.at += if negated { 2 } else { 1 };
            self.expect("(")?;
            let list = self.list()?;
            self.expect(")")?;
            if list.is_empty() {
                return Err(Error::Invalid("'in' needs at least one value".into()));
            }
            return Ok(Expr::In(Box::new(left), list, negated));
        }
        Ok(left)
    }

    fn sum(&mut self) -> Result<Expr> {
        let mut e = self.product()?;
        loop {
            let op = if self.op("+") {
                "+"
            } else if self.op("-") {
                "-"
            } else if self.op("&") {
                "&"
            } else {
                break;
            };
            self.at += 1;
            e = Expr::Binary(op, Box::new(e), Box::new(self.product()?));
        }
        Ok(e)
    }

    fn product(&mut self) -> Result<Expr> {
        let mut e = self.unary()?;
        loop {
            let op = if self.op("*") {
                "*"
            } else if self.op("/") {
                "/"
            } else if self.op("%") {
                "%"
            } else {
                break;
            };
            self.at += 1;
            e = Expr::Binary(op, Box::new(e), Box::new(self.unary()?));
        }
        Ok(e)
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.op("-") {
            self.at += 1;
            self.deeper()?;
            let e = Expr::Unary("-", Box::new(self.unary()?));
            self.depth -= 1;
            return Ok(e);
        }
        self.atom()
    }

    fn list(&mut self) -> Result<Vec<Expr>> {
        let mut out = Vec::new();
        if self.op(")") {
            return Ok(out);
        }
        loop {
            out.push(self.formula()?);
            if self.op(",") {
                self.at += 1;
            } else {
                return Ok(out);
            }
        }
    }

    fn atom(&mut self) -> Result<Expr> {
        let Some(t) = self.peek().cloned() else { return Err(Error::Invalid("the formula ends too soon".into())) };
        self.at += 1;
        match t {
            Tok::Number(n) => Ok(Expr::Number(n)),
            Tok::Text(s) => Ok(Expr::Text(s)),
            Tok::Bracketed(n) => Ok(Expr::Column(n)),
            Tok::Op("(") => {
                let e = self.formula()?;
                self.expect(")")?;
                Ok(e)
            }
            Tok::Word(w) => {
                if self.op("(") {
                    self.at += 1;
                    let args = self.list()?;
                    self.expect(")")?;
                    return Ok(Expr::Call(w.to_ascii_lowercase(), args));
                }
                match w.to_ascii_lowercase().as_str() {
                    "true" => Ok(Expr::Bool(true)),
                    "false" => Ok(Expr::Bool(false)),
                    "null" => Ok(Expr::Null),
                    "and" | "or" | "not" | "is" | "in" => Err(Error::Invalid(format!("'{w}' is not expected here"))),
                    _ => Ok(Expr::Column(w)),
                }
            }
            Tok::Op(o) => Err(Error::Invalid(format!("'{o}' is not expected here"))),
        }
    }
}

// ------------------------------------------------------------- types

/// A typed piece of SQL; `None` is a bare null, which takes the type
/// of whatever it meets.
type Typed = (String, Option<ColumnType>);

fn typed(e: &Expr, scope: &Scope<'_>, reads: &mut Vec<String>) -> Result<Typed> {
    Ok(match e {
        Expr::Number(n) => {
            let ty = if n.contains(['e', 'E']) {
                Double
            } else if let Some((_, f)) = n.split_once('.') {
                if f.len() as u8 > DECIMAL_SCALE_MAX {
                    Double
                } else {
                    Decimal(f.len() as u8)
                }
            } else if n.parse::<i64>().is_ok() {
                Integer
            } else {
                Double
            };
            (format!("CAST({n} AS {})", ty.sql()), Some(ty))
        }
        Expr::Text(s) => (literal(s), Some(Text)),
        Expr::Bool(b) => (if *b { "TRUE" } else { "FALSE" }.into(), Some(Boolean)),
        Expr::Null => ("NULL".into(), None),
        Expr::Column(name) => {
            let c = scope
                .columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| Error::Invalid(format!("there is no column '{name}' here{}", nearest(name, scope.columns))))?;
            reads.push(c.name.clone());
            (ident(&c.name), Some(c.ty))
        }
        Expr::Unary("not", a) => {
            let (sql, t) = typed(a, scope, reads)?;
            want(t, Boolean, "not")?;
            (format!("(NOT {sql})"), Some(Boolean))
        }
        Expr::Unary(_, a) => {
            let (sql, t) = typed(a, scope, reads)?;
            if t.is_some_and(|t| !t.is_number()) {
                return Err(Error::Invalid(format!("'-' takes a number, not {}", t.unwrap_or(Text))));
            }
            (format!("(-{sql})"), t)
        }
        Expr::IsNull(a, negated) => {
            let (sql, _) = typed(a, scope, reads)?;
            (format!("({sql} IS {}NULL)", if *negated { "NOT " } else { "" }), Some(Boolean))
        }
        Expr::In(a, list, negated) => {
            let (sql, t) = typed(a, scope, reads)?;
            let mut items = Vec::new();
            for x in list {
                let (s, u) = typed(x, scope, reads)?;
                meet_or_refuse(t, u, "in")?;
                items.push(s);
            }
            (format!("({sql} {}IN ({}))", if *negated { "NOT " } else { "" }, items.join(", ")), Some(Boolean))
        }
        Expr::Binary(op, a, b) => {
            let (l, lt) = typed(a, scope, reads)?;
            let (r, rt) = typed(b, scope, reads)?;
            binary(op, l, lt, r, rt)?
        }
        Expr::Call(name, args) => {
            let mut sqls = Vec::new();
            let mut types = Vec::new();
            for a in args {
                let (s, t) = typed(a, scope, reads)?;
                sqls.push(s);
                types.push(t);
            }
            call(name, &sqls, &types, args, scope)?
        }
    })
}

fn nearest(name: &str, columns: &[Column]) -> String {
    let lower = name.to_lowercase();
    let close: Vec<&str> = columns
        .iter()
        .map(|c| c.name.as_str())
        .filter(|c| {
            let l = c.to_lowercase();
            l.contains(&lower) || lower.contains(&l)
        })
        .take(3)
        .collect();
    if close.is_empty() {
        String::new()
    } else {
        format!("; the nearest are {}", close.iter().map(|c| format!("'{c}'")).collect::<Vec<_>>().join(", "))
    }
}

fn want(t: Option<ColumnType>, wanted: ColumnType, what: &str) -> Result<()> {
    match t {
        Some(t) if t != wanted => Err(Error::Invalid(format!("{what} takes {wanted}, not {t}"))),
        _ => Ok(()),
    }
}

fn want_number(t: Option<ColumnType>, what: &str) -> Result<()> {
    match t {
        Some(t) if !t.is_number() => Err(Error::Invalid(format!("{what} takes a number, not {t}"))),
        _ => Ok(()),
    }
}

fn want_text(t: Option<ColumnType>, what: &str) -> Result<()> {
    want(t, Text, what)
}

fn want_date(t: Option<ColumnType>, what: &str) -> Result<()> {
    match t {
        Some(Date | Timestamp) | None => Ok(()),
        Some(t) => Err(Error::Invalid(format!("{what} takes a date or a timestamp, not {t}"))),
    }
}

/// The type two values meet at, for a comparison, an `in` or a branch.
fn meet_or_refuse(a: Option<ColumnType>, b: Option<ColumnType>, what: &str) -> Result<Option<ColumnType>> {
    match (a, b) {
        (None, x) | (x, None) => Ok(x),
        (Some(a), Some(b)) => common(a, b)
            .map(Some)
            .or_else(|| (a.is_number() && b.is_number()).then_some(Some(Double)))
            .ok_or_else(|| Error::Invalid(format!("{what} meets {a} with {b}, which do not compare"))),
    }
}

/// The type of arithmetic on two numbers.
fn arithmetic(op: &str, a: ColumnType, b: ColumnType) -> ColumnType {
    match (op, a, b) {
        ("/", _, _) => Double,
        (_, Double, _) | (_, _, Double) => Double,
        ("%", Integer, Integer) => Integer,
        ("%", _, _) => Double,
        (_, Integer, Integer) => Integer,
        ("*", Decimal(x), Decimal(y)) => Decimal((x + y).min(DECIMAL_SCALE_MAX)),
        (_, Decimal(x), Decimal(y)) => Decimal(x.max(y)),
        (_, Decimal(x), _) | (_, _, Decimal(x)) => Decimal(x),
        _ => Double,
    }
}

fn binary(op: &'static str, l: String, lt: Option<ColumnType>, r: String, rt: Option<ColumnType>) -> Result<Typed> {
    Ok(match op {
        "and" | "or" => {
            want(lt, Boolean, op)?;
            want(rt, Boolean, op)?;
            (format!("({l} {} {r})", op.to_uppercase()), Some(Boolean))
        }
        "=" | "!=" | "<" | "<=" | ">" | ">=" => {
            meet_or_refuse(lt, rt, &format!("'{op}'"))?;
            (format!("({l} {} {r})", if op == "!=" { "<>" } else { op }), Some(Boolean))
        }
        "&" => (format!("(COALESCE(CAST({l} AS VARCHAR), '') || COALESCE(CAST({r} AS VARCHAR), ''))"), Some(Text)),
        "+" | "-" => match (lt, rt) {
            // A date moved by a number of days, and the days between two.
            (Some(Date), Some(Integer)) => (format!("({l} {op} CAST({r} AS INTEGER))"), Some(Date)),
            (Some(Integer), Some(Date)) if op == "+" => (format!("({r} + CAST({l} AS INTEGER))"), Some(Date)),
            (Some(Date), Some(Date)) if op == "-" => (format!("CAST(({l} - {r}) AS BIGINT)"), Some(Integer)),
            _ => {
                want_number(lt, &format!("'{op}'"))?;
                want_number(rt, &format!("'{op}'"))?;
                let ty = arithmetic(op, lt.unwrap_or(Integer), rt.unwrap_or(Integer));
                (format!("CAST(({l} {op} {r}) AS {})", ty.sql()), Some(ty))
            }
        },
        _ => {
            want_number(lt, &format!("'{op}'"))?;
            want_number(rt, &format!("'{op}'"))?;
            let ty = arithmetic(op, lt.unwrap_or(Integer), rt.unwrap_or(Integer));
            let sql = match op {
                // A division by zero is a null, as in a spreadsheet's
                // blank, not a failed run.
                "/" => format!("CAST(CAST({l} AS DOUBLE) / NULLIF(CAST({r} AS DOUBLE), 0) AS DOUBLE)"),
                "%" => format!("CAST(({l} % NULLIF({r}, 0)) AS {})", ty.sql()),
                _ => format!("CAST(({l} {op} {r}) AS {})", ty.sql()),
            };
            (sql, Some(ty))
        }
    })
}

/// The parts of a date `date_trunc` and `date_diff` take.
const PARTS: [&str; 5] = ["year", "quarter", "month", "week", "day"];

fn part_of(args: &[Expr], i: usize, f: &str) -> Result<String> {
    match args.get(i) {
        Some(Expr::Text(p)) if PARTS.contains(&p.to_lowercase().as_str()) => Ok(p.to_lowercase()),
        _ => Err(Error::Invalid(format!("{f} takes a part as its first argument, written as text: one of {}", PARTS.join(", ")))),
    }
}

/// The formats `to_date` reads, as strptime writes them.
/// A text value's words, as a list: split at anything not a letter or a
/// digit, and where a lower-case letter or a digit meets an upper-case
/// one, so `GroceryStore` and `grocery-store` are both two words.
fn words(x: &str) -> String {
    format!(
        r"string_split(trim(regexp_replace(regexp_replace({x}, '([\p{{Ll}}\pN])(\p{{Lu}})', '\1 \2', 'g'), '[^\pL\pN]+', ' ', 'g')), ' ')"
    )
}

/// `Grocery Store`: each word's first letter upper case, the rest lower.
pub(crate) fn title_case(x: &str) -> String {
    format!("array_to_string(list_transform(string_split({x}, ' '), lambda w: upper(w[1]) || lower(w[2:])), ' ')")
}

/// `grocery_store`, from `Grocery Store`, `GroceryStore` or `grocery-store`.
pub(crate) fn snake_case(x: &str) -> String {
    format!("lower(array_to_string({}, '_'))", words(x))
}

/// `groceryStore`.
pub(crate) fn camel_case(x: &str) -> String {
    format!("lower({w}[1]) || array_to_string(list_transform({w}[2:], lambda v: upper(v[1]) || lower(v[2:])), '')", w = words(x))
}

/// What `to_number` gives: exact, with room for a percentage's places.
const NUMBER: ColumnType = Decimal(6);

/// The decimal mark a number is written with: `.` unless the argument
/// says `,`. Never guessed: `1,234` is a thousand in one file and a
/// little over one in another.
fn decimal_mark(args: &[Expr], i: usize, f: &str) -> Result<char> {
    match args.get(i) {
        None => Ok('.'),
        Some(Expr::Text(m)) if m == "." || m == "," => Ok(m.chars().next().unwrap_or('.')),
        _ => Err(Error::Invalid(format!("{f}()'s decimal mark is '.' or ','"))),
    }
}

/// A value read as a number the way people write one: a currency symbol
/// or a three-letter code at either end, spaces and the thousands
/// separator dropped; `(500)` is -500 and `12%` is 0.12. Anything else
/// left over is no number, and gives null.
fn number_of(x: &str, mark: char) -> String {
    let (thousands, to_dot) = if mark == ',' { (".", "replace(c, ',', '.')") } else { (",", "c") };
    let stripped = format!(
        r"regexp_replace(regexp_replace(trim(CAST({x} AS VARCHAR)), '^\p{{Lu}}{{3}}[\s\x{{00A0}}]*|[\s\x{{00A0}}]*\p{{Lu}}{{3}}$', ''), '[\p{{Sc}}\s\x{{00A0}}\x{{202F}}{thousands}'']', '', 'g')"
    );
    format!(
        r"list_transform([{stripped}], lambda c: CAST((CASE WHEN starts_with(c, '(') AND regexp_matches(c, '\)%?$') THEN -1 ELSE 1 END) * TRY_CAST(regexp_replace({to_dot}, '[()%]', '', 'g') AS {d}) * (CASE WHEN regexp_matches(c, '%\)?$') THEN 0.01 ELSE 1 END) AS {d}))[1]",
        d = NUMBER.sql()
    )
}

/// How `to_date` reads its text: every unambiguous way a date is
/// written; and also, when asked, a day before its month or a month
/// before its day, a spreadsheet's serial number, or one format.
enum DateHow {
    Auto,
    DayFirst,
    MonthFirst,
    Excel,
    Format(String),
}

fn date_how(args: &[Expr], i: usize, f: &str) -> Result<DateHow> {
    match args.get(i) {
        None => Ok(DateHow::Auto),
        Some(Expr::Text(h)) => match h.to_lowercase().as_str() {
            "dmy" => Ok(DateHow::DayFirst),
            "mdy" => Ok(DateHow::MonthFirst),
            "excel" => Ok(DateHow::Excel),
            _ if h.contains('%') && h.len() <= 64 => Ok(DateHow::Format(h.clone())),
            _ => Err(bad_how(f)),
        },
        _ => Err(bad_how(f)),
    }
}

fn bad_how(f: &str) -> Error {
    Error::Invalid(format!(
        "{f}()'s second argument is 'dmy' (31/01/2026), 'mdy' (01/31/2026), 'excel' (a spreadsheet's serial number) or a format in strptime's form, such as '%d/%m/%Y'"
    ))
}

/// The ways of writing a date with a month's name.
const MONTH_NAMED: &str =
    "['%d %b %Y', '%d %B %Y', '%b %d, %Y', '%B %d, %Y', '%b %d %Y', '%B %d %Y', '%d-%b-%Y', '%d-%B-%Y', '%b %Y', '%B %Y']";

/// A value read as a date. Each reading is a whole match, and a date that
/// does not exist (`2026-13-01`) is null, never another date.
fn date_of(x: &str, how: &DateHow) -> String {
    let text = format!("trim(CAST({x} AS VARCHAR))");
    if let DateHow::Format(f) = how {
        return format!("TRY_CAST(try_strptime({text}, {}) AS DATE)", literal(f));
    }
    // ISO dates and timestamps; year first with dots; YYYYMMDD, YYYYMM,
    // YYYY-MM; a month's name.
    let auto = format!(
        r"COALESCE(TRY_CAST(s AS DATE), CASE
         WHEN regexp_matches(s, '^\d{{4}}\.\d{{1,2}}\.\d{{1,2}}$') THEN TRY_CAST(replace(s, '.', '-') AS DATE)
         WHEN regexp_matches(s, '^\d{{8}}$') THEN TRY_CAST(try_strptime(s, '%Y%m%d') AS DATE)
         WHEN regexp_matches(s, '^\d{{6}}$') THEN TRY_CAST(try_strptime(s, '%Y%m') AS DATE)
         WHEN regexp_matches(s, '^\d{{4}}[-/.]\d{{1,2}}$') THEN TRY_CAST(try_strptime(regexp_replace(s, '[/.]', '-'), '%Y-%m') AS DATE)
         WHEN regexp_matches(s, '\pL') THEN TRY_CAST(try_strptime(s, {MONTH_NAMED}) AS DATE) END)"
    );
    let extra = match how {
        DateHow::DayFirst | DateHow::MonthFirst => format!(
            r"CASE WHEN regexp_matches(s, '^\d{{1,2}}[-/.]\d{{1,2}}[-/.]\d{{4}}$') THEN TRY_CAST(try_strptime(regexp_replace(s, '[-.]', '/', 'g'), '{}') AS DATE) END",
            if matches!(how, DateHow::DayFirst) { "%d/%m/%Y" } else { "%m/%d/%Y" }
        ),
        // Days since 1899-12-30, as Excel and Sheets count them: five
        // digits at most, which is to 2173, so a mistyped YYYYMM is not
        // a date centuries away.
        DateHow::Excel => r"CASE WHEN regexp_matches(s, '^\d{1,5}(\.\d+)?$') AND TRY_CAST(s AS DOUBLE) >= 1
             THEN CAST(DATE '1899-12-30' + CAST(floor(TRY_CAST(s AS DOUBLE)) AS INTEGER) AS DATE) END"
            .to_string(),
        _ => "NULL".to_string(),
    };
    format!("list_transform([{text}], lambda s: COALESCE({auto}, {extra}))[1]")
}

/// A value read as a timestamp: as `to_date` reads a date, at midnight,
/// and with its time where it has one: ISO, a day-first or month-first
/// date followed by `HH:MM` or `HH:MM:SS`, an Excel serial's fraction.
fn timestamp_of(x: &str, how: &DateHow) -> String {
    let text = format!("trim(CAST({x} AS VARCHAR))");
    if let DateHow::Format(f) = how {
        return format!("TRY_CAST(try_strptime({text}, {}) AS TIMESTAMP)", literal(f));
    }
    let timed = match how {
        DateHow::DayFirst | DateHow::MonthFirst => {
            let d = if matches!(how, DateHow::DayFirst) { "%d/%m/%Y" } else { "%m/%d/%Y" };
            format!(
                r"CASE WHEN regexp_matches(s, '^\d{{1,2}}[-/.]\d{{1,2}}[-/.]\d{{4}}[ T]\d{{1,2}}:\d{{2}}(:\d{{2}})?$') THEN TRY_CAST(try_strptime(regexp_replace(regexp_replace(s, '[-.]', '/', 'g'), 'T', ' '), ['{d} %H:%M:%S', '{d} %H:%M']) AS TIMESTAMP) END"
            )
        }
        DateHow::Excel => r"CASE WHEN regexp_matches(s, '^\d{1,5}(\.\d+)?$') AND TRY_CAST(s AS DOUBLE) >= 1
             THEN CAST(DATE '1899-12-30' AS TIMESTAMP) + to_microseconds(CAST(round(TRY_CAST(s AS DOUBLE) * 86400000000) AS BIGINT)) END"
            .to_string(),
        _ => "NULL".to_string(),
    };
    format!(
        "COALESCE(list_transform([{text}], lambda s: COALESCE(TRY_CAST(s AS TIMESTAMP), {timed}))[1], CAST({} AS TIMESTAMP))",
        date_of(x, how)
    )
}

/// The allowlist. A function not here is not a function.
const FUNCTIONS: [&str; 52] = [
    "upper",
    "lower",
    "title_case",
    "snake_case",
    "camel_case",
    "trim",
    "length",
    "left",
    "right",
    "substr",
    "replace",
    "concat",
    "contains",
    "starts_with",
    "ends_with",
    "split_part",
    "abs",
    "round",
    "floor",
    "ceil",
    "power",
    "sqrt",
    "least",
    "greatest",
    "today",
    "now",
    "year",
    "month",
    "day",
    "quarter",
    "week",
    "date_trunc",
    "date_diff",
    "make_date",
    "if",
    "ifs",
    "switch",
    "coalesce",
    "nullif",
    "to_text",
    "to_number",
    "to_integer",
    "to_date",
    "to_timestamp",
    "to_boolean",
    "is_number",
    "is_date",
    "weekday",
    "ln",
    "exp",
    "sign",
    "log10",
];

/// The longest value a `replace()` that lengthens may make. Nested, each
/// one multiplies the length, and the engine's memory limit does not
/// hold back a value that size: it is refused before it is made.
pub const REPLACE_LIMIT: usize = 64 * 1024;

/// `replace(x, a, b)`, refused with words where it could make a value
/// past [`REPLACE_LIMIT`]: at most `length(x) * (1 + length(b))` long
/// when `b` is longer than `a`. Each argument is written once, bound in
/// a lambda, so nesting does not double the SQL.
fn replace_within(x: &str, a: &str, b: &str) -> String {
    format!(
        "list_transform([{{'x': {x}, 'a': {a}, 'b': {b}}}], lambda r: CASE \
         WHEN coalesce(length(r.b), 0) <= coalesce(length(r.a), 0) \
         OR coalesce(length(r.x), 0) * (1 + coalesce(length(r.b), 0)) <= {REPLACE_LIMIT} THEN replace(r.x, r.a, r.b) \
         ELSE error('replace() would make a value longer than {REPLACE_LIMIT} characters') END)[1]"
    )
}

fn arity(name: &str, n: usize, min: usize, max: usize) -> Result<()> {
    if n < min || n > max {
        let want = if min == max {
            format!("{min}")
        } else if max == usize::MAX {
            format!("at least {min}")
        } else {
            format!("{min} to {max}")
        };
        return Err(Error::Invalid(format!("{name}() takes {want} argument(s), not {n}")));
    }
    Ok(())
}

fn call(name: &str, a: &[String], t: &[Option<ColumnType>], args: &[Expr], scope: &Scope<'_>) -> Result<Typed> {
    if !FUNCTIONS.contains(&name) {
        return Err(Error::Invalid(format!("{name}() is not a function here; the functions are {}", FUNCTIONS.join(", "))));
    }
    let n = a.len();
    let text1 = |f: &str| -> Result<()> {
        arity(f, n, 1, 1)?;
        want_text(t[0], &format!("{f}()"))
    };
    Ok(match name {
        "upper" | "lower" | "trim" => {
            text1(name)?;
            (format!("{}({})", name, a[0]), Some(Text))
        }
        "title_case" | "snake_case" | "camel_case" => {
            text1(name)?;
            let sql = match name {
                "title_case" => title_case(&a[0]),
                "snake_case" => snake_case(&a[0]),
                _ => camel_case(&a[0]),
            };
            (sql, Some(Text))
        }
        "length" => {
            text1(name)?;
            (format!("CAST(length({}) AS BIGINT)", a[0]), Some(Integer))
        }
        "left" | "right" => {
            arity(name, n, 2, 2)?;
            want_text(t[0], &format!("{name}()"))?;
            want(t[1], Integer, &format!("{name}()'s count"))?;
            (format!("{name}({}, CAST({} AS BIGINT))", a[0], a[1]), Some(Text))
        }
        "substr" => {
            arity(name, n, 2, 3)?;
            want_text(t[0], "substr()")?;
            for x in &t[1..] {
                want(*x, Integer, "substr()'s position and length")?;
            }
            (format!("substr({})", a.join(", ")), Some(Text))
        }
        "replace" => {
            arity(name, n, 3, 3)?;
            for x in t {
                want_text(*x, "replace()")?;
            }
            (replace_within(&a[0], &a[1], &a[2]), Some(Text))
        }
        "concat" => {
            arity(name, n, 1, usize::MAX)?;
            (format!("concat({})", a.iter().map(|x| format!("CAST({x} AS VARCHAR)")).collect::<Vec<_>>().join(", ")), Some(Text))
        }
        "contains" | "starts_with" | "ends_with" => {
            arity(name, n, 2, 2)?;
            want_text(t[0], &format!("{name}()"))?;
            want_text(t[1], &format!("{name}()"))?;
            let f = match name {
                "contains" => "contains",
                "starts_with" => "starts_with",
                _ => "suffix",
            };
            (format!("{f}({}, {})", a[0], a[1]), Some(Boolean))
        }
        "split_part" => {
            arity(name, n, 3, 3)?;
            want_text(t[0], "split_part()")?;
            want_text(t[1], "split_part()'s delimiter")?;
            want(t[2], Integer, "split_part()'s index")?;
            (format!("split_part({}, {}, CAST({} AS BIGINT))", a[0], a[1], a[2]), Some(Text))
        }
        "abs" | "sign" => {
            arity(name, n, 1, 1)?;
            want_number(t[0], &format!("{name}()"))?;
            let ty = if name == "sign" { Integer } else { t[0].unwrap_or(Double) };
            (format!("CAST({name}({}) AS {})", a[0], ty.sql()), Some(ty))
        }
        "round" => {
            arity(name, n, 1, 2)?;
            want_number(t[0], "round()")?;
            let places = match args.get(1) {
                None => 0,
                Some(Expr::Number(p)) => p
                    .parse::<u8>()
                    .ok()
                    .filter(|p| *p <= DECIMAL_SCALE_MAX)
                    .ok_or_else(|| Error::Invalid(format!("round()'s places is a whole number from 0 to {DECIMAL_SCALE_MAX}")))?,
                _ => return Err(Error::Invalid("round()'s places is written as a number".into())),
            };
            let ty = match t[0] {
                Some(Double) => Double,
                Some(Integer) | None if places == 0 => Integer,
                _ if places == 0 => Integer,
                _ => Decimal(places),
            };
            (format!("CAST(round({}, {places}) AS {})", a[0], ty.sql()), Some(ty))
        }
        "floor" | "ceil" => {
            arity(name, n, 1, 1)?;
            want_number(t[0], &format!("{name}()"))?;
            (format!("CAST({name}({}) AS BIGINT)", a[0]), Some(Integer))
        }
        "power" => {
            arity(name, n, 2, 2)?;
            want_number(t[0], "power()")?;
            want_number(t[1], "power()")?;
            (format!("CAST(power(CAST({} AS DOUBLE), CAST({} AS DOUBLE)) AS DOUBLE)", a[0], a[1]), Some(Double))
        }
        "sqrt" | "ln" | "exp" | "log10" => {
            arity(name, n, 1, 1)?;
            want_number(t[0], &format!("{name}()"))?;
            // Out of a function's domain is a null, not a failed run.
            let guarded = match name {
                "sqrt" => format!("CASE WHEN {0} >= 0 THEN sqrt(CAST({0} AS DOUBLE)) END", a[0]),
                "ln" | "log10" => format!("CASE WHEN {0} > 0 THEN {name}(CAST({0} AS DOUBLE)) END", a[0]),
                _ => format!("exp(CAST({} AS DOUBLE))", a[0]),
            };
            (format!("CAST({guarded} AS DOUBLE)"), Some(Double))
        }
        "least" | "greatest" | "coalesce" => {
            arity(name, n, 1, usize::MAX)?;
            let mut ty: Option<ColumnType> = None;
            for x in t {
                ty = meet_or_refuse(ty, *x, &format!("{name}()"))?;
            }
            let ty = ty.unwrap_or(Text);
            (
                format!(
                    "CAST({name}({}) AS {})",
                    a.iter().map(|x| format!("CAST({x} AS {})", ty.sql())).collect::<Vec<_>>().join(", "),
                    ty.sql()
                ),
                Some(ty),
            )
        }
        "nullif" => {
            arity(name, n, 2, 2)?;
            let ty = meet_or_refuse(t[0], t[1], "nullif()")?.unwrap_or(Text);
            (format!("CAST(nullif({}, {}) AS {})", a[0], a[1], ty.sql()), Some(ty))
        }
        "if" => {
            arity(name, n, 2, 3)?;
            want(t[0], Boolean, "if()'s condition")?;
            let otherwise = a.get(2).cloned().unwrap_or_else(|| "NULL".into());
            let ty = meet_or_refuse(t[1], t.get(2).copied().flatten(), "if()")?.unwrap_or(Text);
            (format!("CAST(CASE WHEN {} THEN {} ELSE {} END AS {})", a[0], a[1], otherwise, ty.sql()), Some(ty))
        }
        // switch(x, v1, r1, v2, r2, ..., default?): the result for the
        // first value x equals; the default, or null, for none.
        "switch" => {
            if n < 3 {
                return Err(Error::Invalid(
                    "switch() takes a value, then pairs of a value to match and its result, then a default if wanted: switch(region, 'NY', 'New York', region)".into(),
                ));
            }
            let pairs = (n - 1) / 2;
            let default = n.is_multiple_of(2).then(|| n - 1);
            let mut key = t[0];
            for k in 0..pairs {
                key = meet_or_refuse(key, t[1 + 2 * k], "switch()'s values")?;
            }
            let mut ty: Option<ColumnType> = None;
            for i in (0..pairs).map(|k| 2 + 2 * k).chain(default) {
                ty = meet_or_refuse(ty, t[i], "switch()'s results")?;
            }
            let (key, ty) = (key.unwrap_or(Text), ty.unwrap_or(Text));
            let arms: Vec<String> = (0..pairs)
                .map(|k| format!("WHEN CAST({} AS {}) THEN CAST({} AS {})", a[1 + 2 * k], key.sql(), a[2 + 2 * k], ty.sql()))
                .collect();
            let otherwise = default.map(|i| format!(" ELSE CAST({} AS {})", a[i], ty.sql())).unwrap_or_default();
            (format!("CAST(CASE CAST({} AS {}) {}{otherwise} END AS {})", a[0], key.sql(), arms.join(" "), ty.sql()), Some(ty))
        }
        // ifs(c1, r1, c2, r2, ..., default?): the result of the first true
        // condition; the default, or null, for none.
        "ifs" => {
            if n < 2 {
                return Err(Error::Invalid(
                    "ifs() takes pairs of a condition and its result, then a default if wanted: ifs(amount > 100, 'big', amount > 10, 'mid', 'small')".into(),
                ));
            }
            let pairs = n / 2;
            let default = (!n.is_multiple_of(2)).then(|| n - 1);
            let mut ty: Option<ColumnType> = None;
            for k in 0..pairs {
                want(t[2 * k], Boolean, "ifs()'s conditions")?;
                ty = meet_or_refuse(ty, t[2 * k + 1], "ifs()'s results")?;
            }
            if let Some(i) = default {
                ty = meet_or_refuse(ty, t[i], "ifs()'s results")?;
            }
            let ty = ty.unwrap_or(Text);
            let arms: Vec<String> = (0..pairs).map(|k| format!("WHEN {} THEN CAST({} AS {})", a[2 * k], a[2 * k + 1], ty.sql())).collect();
            let otherwise = default.map(|i| format!(" ELSE CAST({} AS {})", a[i], ty.sql())).unwrap_or_default();
            (format!("CAST(CASE {}{otherwise} END AS {})", arms.join(" "), ty.sql()), Some(ty))
        }
        "to_boolean" => {
            arity(name, n, 1, 1)?;
            (
                format!(
                    "CASE WHEN lower(trim(CAST({x} AS VARCHAR))) IN ('true', 't', 'yes', 'y', '1') THEN TRUE \
                     WHEN lower(trim(CAST({x} AS VARCHAR))) IN ('false', 'f', 'no', 'n', '0') THEN FALSE END",
                    x = a[0]
                ),
                Some(Boolean),
            )
        }
        "to_timestamp" => {
            arity(name, n, 1, 2)?;
            let how = date_how(args, 1, name)?;
            if matches!(how, DateHow::Format(_)) {
                want_text(t[0], "to_timestamp() with a format")?;
            }
            (timestamp_of(&a[0], &how), Some(Timestamp))
        }
        "today" => {
            arity(name, n, 0, 0)?;
            (format!("DATE '{}'", &scope.pinned_at[..10]), Some(Date))
        }
        "now" => {
            arity(name, n, 0, 0)?;
            (format!("TIMESTAMP '{} {}'", &scope.pinned_at[..10], &scope.pinned_at[11..23]), Some(Timestamp))
        }
        "year" | "month" | "day" | "quarter" | "week" | "weekday" => {
            arity(name, n, 1, 1)?;
            want_date(t[0], &format!("{name}()"))?;
            let f = match name {
                "weekday" => "isodow",
                "week" => "weekofyear",
                other => other,
            };
            (format!("CAST({f}({}) AS BIGINT)", a[0]), Some(Integer))
        }
        "date_trunc" => {
            arity(name, n, 2, 2)?;
            let part = part_of(args, 0, "date_trunc()")?;
            want_date(t[1], "date_trunc()")?;
            let ty = t[1].unwrap_or(Date);
            (format!("CAST(date_trunc('{part}', {}) AS {})", a[1], ty.sql()), Some(ty))
        }
        "date_diff" => {
            arity(name, n, 3, 3)?;
            let part = part_of(args, 0, "date_diff()")?;
            want_date(t[1], "date_diff()")?;
            want_date(t[2], "date_diff()")?;
            (format!("CAST(date_diff('{part}', {}, {}) AS BIGINT)", a[1], a[2]), Some(Integer))
        }
        "make_date" => {
            arity(name, n, 3, 3)?;
            for x in t {
                want(*x, Integer, "make_date()")?;
            }
            (
                format!("TRY_CAST(make_date(CAST({} AS BIGINT), CAST({} AS BIGINT), CAST({} AS BIGINT)) AS DATE)", a[0], a[1], a[2]),
                Some(Date),
            )
        }
        "to_text" => {
            arity(name, n, 1, 1)?;
            (format!("CAST({} AS VARCHAR)", a[0]), Some(Text))
        }
        "to_number" | "to_integer" | "is_number" => {
            arity(name, n, 1, 2)?;
            let parsed = number_of(&a[0], decimal_mark(args, 1, name)?);
            match name {
                "to_number" => (parsed, Some(NUMBER)),
                // A whole number, or none: 1.5 is not an integer rounded.
                "to_integer" => (
                    format!("list_transform([{parsed}], lambda n: CASE WHEN n = trunc(n) THEN TRY_CAST(n AS BIGINT) END)[1]"),
                    Some(Integer),
                ),
                _ => (format!("({parsed} IS NOT NULL)"), Some(Boolean)),
            }
        }
        "to_date" | "is_date" => {
            arity(name, n, 1, 2)?;
            let how = date_how(args, 1, name)?;
            if matches!(how, DateHow::Format(_)) {
                want_text(t[0], &format!("{name}() with a format"))?;
            }
            let parsed = date_of(&a[0], &how);
            if name == "is_date" {
                (format!("({parsed} IS NOT NULL)"), Some(Boolean))
            } else {
                (parsed, Some(Date))
            }
        }
        _ => return Err(Error::Internal(format!("{name}() is allowed and not compiled"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::engine;
    use crate::schema::Role;

    fn columns() -> Vec<Column> {
        [("region", Text), ("amount", Decimal(2)), ("units", Integer), ("month", Date), ("order date", Date), ("ok", Boolean)]
            .into_iter()
            .map(|(n, t)| Column { name: n.to_string(), ty: t, role: Role::Dimension })
            .collect()
    }

    fn compiled(f: &str) -> Result<Compiled> {
        let cols = columns();
        compile(f, &Scope { columns: &cols, pinned_at: "2026-11-26T10:20:30.400Z" })
    }

    /// A formula's value over one row, through the engine.
    fn value(f: &str) -> String {
        let c = compiled(f).unwrap();
        let s = engine().session().unwrap();
        s.execute(
            "CREATE TABLE t AS SELECT 'EMEA' AS region, 12.50::DECIMAL(38,2) AS amount, 3::BIGINT AS units, \
             DATE '2026-01-31' AS month, DATE '2026-03-01' AS \"order date\", TRUE AS ok",
        )
        .unwrap();
        s.texts(&format!("SELECT CAST({} AS VARCHAR) FROM t", c.sql)).unwrap().pop().unwrap_or_else(|| "NULL".into())
    }

    #[test]
    fn formulas_are_typed_before_they_run() {
        assert_eq!(compiled("amount * units").unwrap().ty, Decimal(2));
        assert_eq!(compiled("amount * 1.5").unwrap().ty, Decimal(3));
        assert_eq!(compiled("units + 1").unwrap().ty, Integer);
        assert_eq!(compiled("units / 2").unwrap().ty, Double);
        assert_eq!(compiled("region & '-' & units").unwrap().ty, Text);
        assert_eq!(compiled("amount > 10 and not ok").unwrap().ty, Boolean);
        assert_eq!(compiled("month + 1").unwrap().ty, Date);
        assert_eq!(compiled("[order date] - month").unwrap().ty, Integer);
        assert_eq!(compiled("if(ok, units, 0.5)").unwrap().ty, Decimal(1));
        assert_eq!(compiled("year(month)").unwrap().reads, ["month"]);
        assert_eq!(compiled("REGION = 'x'").unwrap().reads, ["region"], "names are case-insensitive");
    }

    #[test]
    fn a_formula_is_refused_plainly() {
        for (f, says) in [
            ("region + 1", "takes a number"),
            ("nope + 1", "no column 'nope'"),
            ("units = 'x'", "do not compare"),
            ("read_csv('/etc/passwd')", "not a function here"),
            ("upper(units)", "takes text"),
            ("left(region)", "takes 2 argument"),
            ("amount >", "ends too soon"),
            ("(units", "')' is expected"),
            ("'open", "not closed"),
            ("units ; drop", "not part of a formula"),
            ("date_trunc(month, month)", "a part"),
            ("units units", "not expected"),
        ] {
            let e = compiled(f).err().unwrap_or_else(|| panic!("{f} compiled"));
            assert!(e.to_string().contains(says), "{f}: {e}");
        }
        let deep = format!("{}1{}", "(".repeat(100), ")".repeat(100));
        assert!(compiled(&deep).is_err());
    }

    #[test]
    fn formulas_compute_what_they_say() {
        assert_eq!(value("amount * units"), "37.50");
        assert_eq!(value("units / 0"), "NULL");
        assert_eq!(value("upper(region) & ':' & units"), "EMEA:3");
        assert_eq!(value("left(region, 2)"), "EM");
        assert_eq!(value("if(units > 2, 'many', 'few')"), "many");
        assert_eq!(value("region in ('EMEA', 'APAC')"), "true");
        assert_eq!(value("region not in ('APAC')"), "true");
        assert_eq!(value("month is not null"), "true");
        assert_eq!(value("date_trunc('month', [order date])"), "2026-03-01");
        assert_eq!(value("date_diff('day', month, [order date])"), "29");
        assert_eq!(value("month + 1"), "2026-02-01");
        assert_eq!(value("today()"), "2026-11-26");
        assert_eq!(value("year(today())"), "2026");
        assert_eq!(value("round(amount / 3, 2)"), "4.17");
        assert_eq!(value("to_date('31/01/2026', '%d/%m/%Y')"), "2026-01-31");
        assert_eq!(value("to_date('soon')"), "NULL");
        assert_eq!(value("coalesce(null, units, 2)"), "3");
        assert_eq!(value("sqrt(-1)"), "NULL");
        assert_eq!(value("'it''s'"), "it's");
        assert_eq!(value("-units % 2"), "-1");
    }

    #[test]
    fn the_names_a_formula_reads_are_found_without_its_columns() {
        assert_eq!(names("upper(region) & [order date] & year(gone)").unwrap(), ["gone", "order date", "region"]);
    }
}

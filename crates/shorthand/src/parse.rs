//! The parser: the PEG in `docs/expression-language.md`, read top down over
//! the bytes of the line with no lexer. Every rule that can fail softly
//! returns `Ok(None)` and leaves the cursor where it was, which is PEG's
//! ordered choice; every rule past a commit point returns `Err`, which is
//! the message the person reads. Nothing is resolved here: a name is
//! carried as written and the spec lowering has the model.

use serde_json::{Map, Value};

/// A field as written: bare words, `[bracketed]` or `"quoted"`, with the
/// kind suffix if one was given.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub kind: Option<Kind>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Dimension,
    Measure,
}

/// A filter value: the text written, and whether it was quoted, which
/// decides whether the lowering may read it as a number or a boolean.
#[derive(Debug, Clone, PartialEq)]
pub struct Val {
    pub text: String,
    pub quoted: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Ref(Field),
    Calc {
        sql: String,
        data_type: Option<String>,
    },
    Extract {
        part: String,
        inner: Box<Expr>,
    },
    Custom {
        sql: String,
        inner: Box<Expr>,
    },
    /// A decorator function around a field, a calc or another decorator.
    Deco {
        func: String,
        inner: Box<Expr>,
        n: Option<i64>,
        by: Vec<Field>,
        order: Vec<(Field, String)>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Pred {
    /// `op` is the spec's predicate name.
    Cmp {
        op: &'static str,
        value: Val,
    },
    In {
        negated: bool,
        values: Vec<Val>,
    },
    Between {
        low: Val,
        high: Val,
    },
    /// `which` is contains, starts_with or ends_with.
    Like {
        which: &'static str,
        negated: bool,
        value: Val,
    },
    Null {
        negated: bool,
    },
    Top {
        n: i64,
        by: Option<Field>,
    },
    Keyword {
        values: Vec<Val>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Leaf {
    pub field: Field,
    pub pred: Pred,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FilterNode {
    Leaf(Leaf),
    And(Vec<FilterNode>),
    Or(Vec<FilterNode>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub expr: Expr,
    pub alias: Option<String>,
    pub order: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SegmentKind {
    Include,
    Exclude,
    Expand,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SegmentItem {
    Key(Field),
    Measure(Field),
    Filter(FilterNode),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub kind: SegmentKind,
    pub items: Vec<SegmentItem>,
    pub carried: Vec<(Field, Option<String>)>,
    pub name: Option<String>,
    pub apply_to: Vec<Field>,
    pub join: Option<String>,
    pub dedupe: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ItemKind {
    Projection(Projection),
    Filter(FilterNode),
    Segment(Segment),
    Limit(i64),
    Hint(Vec<Field>),
    Raw(Map<String, Value>),
}

/// One item of the line: what it was read as, the raw JSON attached to it
/// if any, and the text it was read from.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub kind: ItemKind,
    pub raw: Option<Map<String, Value>>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub msg: String,
    /// Byte offset into the line.
    pub at: usize,
}

type R<T> = Result<T, ParseError>;
/// A rule that may decline without consuming anything.
type Soft<T> = Result<Option<T>, ParseError>;

const RESERVED: [&str; 14] =
    ["and", "or", "not", "in", "is", "between", "top", "contains", "starts", "ends", "keyword", "as", "asc", "desc"];
const STRUCTURAL: &str = ",()[]{}@=<>!\"";
pub const TRUNCATES: [&str; 10] = ["raw", "millisecond", "second", "minute", "hour", "day", "week", "month", "quarter", "year"];
pub const PARTS: [&str; 12] = [
    "minute",
    "hour",
    "day_of_month",
    "day_of_week",
    "day_of_year",
    "week_of_year",
    "month",
    "quarter",
    "year",
    "day_name",
    "month_name",
    "year_month",
];
pub const TEMPORALS: [&str; 5] = ["yoy", "qoq", "mom", "wow", "dod"];
pub const FRAME_FUNCTIONS: [&str; 5] = ["sum", "avg", "min", "max", "count"];
pub const RANK_FUNCTIONS: [&str; 10] =
    ["rank", "dense_rank", "row_number", "percent_rank", "cume_dist", "ntile", "lag", "lead", "first_value", "last_value"];
pub const TYPES: [&str; 7] = ["string", "integer", "decimal", "date", "date_time", "boolean", "bigint"];

/// Where a bare name is being read, which decides the words that end it.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// The top level: only the reserved words end a name.
    Top,
    /// Inside a function's parentheses: `by` and `order by` end it too.
    Args,
    /// A segment's tail: `for`, `left`, `inner` and `dedupe` end it.
    Tail,
}

/// Whether `func` names a decorator, and which family it belongs to.
pub fn decorator_family(func: &str) -> Option<&'static str> {
    if TRUNCATES.contains(&func) {
        return Some("truncate");
    }
    if TEMPORALS.contains(&func) || TEMPORALS.iter().any(|t| func == format!("{t}_pct")) {
        return Some("temporalize");
    }
    if func == "share" || func == "share_filtered" {
        return Some("contribute");
    }
    if let Some(f) = func.strip_prefix("running_").or_else(|| func.strip_prefix("moving_")) {
        if FRAME_FUNCTIONS.contains(&f) {
            return Some("window");
        }
    }
    if RANK_FUNCTIONS.contains(&func) {
        return Some("window");
    }
    None
}

pub fn parse(line: &str) -> R<Vec<Item>> {
    let mut p = P { s: line, i: 0, depth: 0, last_bare: None, pred_start: 0, nest: 0 };
    let mut items = Vec::new();
    loop {
        p.any_ws();
        if p.eof() {
            break;
        }
        if p.peek() == Some(',') {
            p.i += 1;
            continue;
        }
        let item = p.item()?;
        items.push(item);
        p.ws();
        match p.peek() {
            None => break,
            Some(',') | Some('\n') | Some('\r') => p.i += 1,
            _ => return Err(p.item_end_error(items.last().unwrap())),
        }
    }
    if items.is_empty() {
        return Err(ParseError { msg: "nothing to plan".into(), at: 0 });
    }
    Ok(items)
}

struct P<'a> {
    s: &'a str,
    i: usize,
    /// How many segment groups the cursor is inside, where `)` ends an item.
    depth: usize,
    /// The span of the last bare name read, for the message that says how
    /// to bracket it.
    last_bare: Option<(usize, usize)>,
    /// Where the field of the filter being read began, for the hint that a
    /// reserved word inside a name wants brackets.
    pred_start: usize,
    /// How many parentheses of functions and filter groups are open:
    /// bounded, so a line cannot exhaust the stack.
    nest: usize,
}

/// Parentheses nested deeper than this are refused.
const MAX_NEST: usize = 64;

fn is_word_char(c: char) -> bool {
    !c.is_whitespace() && !STRUCTURAL.contains(c)
}

fn is_word_start(c: char) -> bool {
    is_word_char(c) && c != '\''
}

impl<'a> P<'a> {
    fn rest(&self) -> &'a str {
        &self.s[self.i..]
    }
    fn eof(&self) -> bool {
        self.i >= self.s.len()
    }
    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }
    fn at(&self, lit: &str) -> bool {
        self.rest().starts_with(lit)
    }
    fn eat(&mut self, lit: &str) -> bool {
        if self.at(lit) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }
    /// Spaces and tabs.
    fn ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.i += 1;
        }
    }
    /// At least one space or tab.
    fn sp(&mut self) -> bool {
        let start = self.i;
        self.ws();
        self.i > start
    }
    /// Any whitespace, newlines included.
    fn any_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.i += self.peek().unwrap().len_utf8();
        }
    }
    fn err<T>(&self, at: usize, msg: impl Into<String>) -> R<T> {
        Err(ParseError { msg: msg.into(), at })
    }

    /// The word at the cursor, if one starts here; not consumed.
    fn peek_word(&self) -> Option<&'a str> {
        let rest = self.rest();
        let first = rest.chars().next()?;
        if !is_word_start(first) {
            return None;
        }
        let end = rest.char_indices().find(|(_, c)| !is_word_char(*c)).map(|(i, _)| i).unwrap_or(rest.len());
        Some(&rest[..end])
    }
    fn word(&mut self) -> Option<&'a str> {
        let w = self.peek_word()?;
        self.i += w.len();
        Some(w)
    }
    /// Whether the keyword is at the cursor, as a whole word, in any case.
    fn at_kw(&self, kw: &str) -> bool {
        self.peek_word().is_some_and(|w| w.eq_ignore_ascii_case(kw))
    }
    fn kw(&mut self, kw: &str) -> bool {
        if self.at_kw(kw) {
            self.i += kw.len();
            true
        } else {
            false
        }
    }
    fn reserved_at(&self) -> Option<&'a str> {
        self.peek_word().filter(|w| RESERVED.iter().any(|r| w.eq_ignore_ascii_case(r)))
    }
    fn int(&mut self) -> Option<i64> {
        let w = self.peek_word()?;
        let n = w.parse::<i64>().ok()?;
        self.i += w.len();
        Some(n)
    }

    // ------------------------------------------------------------- items

    fn item(&mut self) -> R<Item> {
        let start = self.i;
        if self.peek() == Some('{') {
            let raw = self.raw()?;
            return Ok(Item { kind: ItemKind::Raw(raw), raw: None, text: self.s[start..self.i].trim().to_string() });
        }
        let kind = if let Some(k) = self.setting()? {
            k
        } else if let Some(k) = self.segment()? {
            k
        } else if let Some(f) = self.filter()? {
            ItemKind::Filter(f)
        } else if let Some(p) = self.projection()? {
            ItemKind::Projection(p)
        } else {
            return self.item_start_error();
        };
        let save = self.i;
        self.ws();
        let raw = if self.peek() == Some('{') {
            if matches!(kind, ItemKind::Limit(_) | ItemKind::Hint(_)) {
                return self.err(self.i, "spec JSON cannot follow a setting; write it as its own item");
            }
            if matches!(&kind, ItemKind::Filter(f) if !matches!(f, FilterNode::Leaf(_))) {
                return self.err(self.i, "spec JSON after a filter attaches to one filter, not to an and/or group");
            }
            Some(self.raw()?)
        } else {
            self.i = save;
            None
        };
        Ok(Item { kind, raw, text: self.s[start..self.i].trim().to_string() })
    }

    fn item_start_error<T>(&self) -> R<T> {
        if let Some(w) = self.reserved_at() {
            let tail = self.name_tail(self.i);
            return self.err(self.i, format!("`{w}` is a reserved word and cannot start a name; write [{tail}] or \"{tail}\""));
        }
        match self.peek() {
            Some('\'') => self.err(self.i, "a single quote opens a value; a field is bare, [bracketed] or \"quoted\""),
            Some(')') => self.err(self.i, "a ) with nothing open before it"),
            Some(c) => self.err(self.i, format!("expected a field, a filter or a setting, found `{c}`")),
            None => self.err(self.i, "expected an item"),
        }
    }

    /// The text from `from` to the end of the item, for a bracketing hint.
    fn name_tail(&self, from: usize) -> String {
        let rest = &self.s[from..];
        let end = rest.find([',', '\n', '\r', ')', '(', '=', '<', '>', '!', '@', '[', '{', '"', '\'']).unwrap_or(rest.len());
        rest[..end].trim().to_string()
    }

    /// What follows an item is a separator or the end; anything else is
    /// one of these mistakes, each with its own message.
    fn item_end_error(&self, item: &Item) -> ParseError {
        let at = self.i;
        let fail = |msg: String| ParseError { msg, at };
        if self.depth > 0 && self.peek() == Some(')') {
            return fail("a ) with nothing open before it".into());
        }
        if let Some(w) = self.reserved_at() {
            if let Some((start, end)) = self.last_bare.filter(|(_, end)| *end == self.s[..at].trim_end().len()) {
                let tail = self.name_tail(start);
                let name = &self.s[start..end];
                return fail(format!("`{w}` is a reserved word and ends the name `{name}`; write [{tail}] or \"{tail}\""));
            }
            return fail(format!("`{w}` is a reserved word and cannot continue the item"));
        }
        match self.peek() {
            Some('(') => {
                if let Some((start, end)) = self.last_bare {
                    let name = &self.s[start..end];
                    return fail(format!(
                        "unknown function `{name}`; a field whose name holds a parenthesis is bracketed. See .help for the functions"
                    ));
                }
                fail("unexpected ( here".into())
            }
            Some('@') => fail("expected @d or @m after the name".into()),
            Some('\'') | Some('"') => fail("a quote opens a name or a value only at the start of a word".into()),
            Some('=' | '<' | '>' | '!') => match &item.kind {
                ItemKind::Projection(p) if !matches!(p.expr, Expr::Ref(_)) => {
                    fail("a filter applies to a field, not to a function; filter on the field itself".into())
                }
                _ => fail("a filter is `field predicate value`; the predicate must follow the field directly".into()),
            },
            Some('{') => fail("spec JSON attaches once per item, after its alias and direction; merge two blocks into one, or start a new item after a comma".into()),
            Some(c) => fail(format!("expected a comma or a new line after `{}`, found `{c}`", item.text)),
            None => fail("expected a comma or a new line".into()),
        }
    }

    // ---------------------------------------------------------- settings

    fn setting(&mut self) -> Soft<ItemKind> {
        let save = self.i;
        if self.kw("limit") && self.sp() {
            let at = self.i;
            return match self.int() {
                Some(n) if n >= 0 && !self.peek().is_some_and(is_word_char) => Ok(Some(ItemKind::Limit(n))),
                _ => self.err(at, "limit needs a whole number of rows"),
            };
        }
        self.i = save;
        if self.kw("hints") && self.sp() {
            if let Some(names) = self.names(Mode::Top)? {
                return Ok(Some(ItemKind::Hint(names)));
            }
        }
        self.i = save;
        Ok(None)
    }

    /// One field, or several in parentheses.
    fn names(&mut self, mode: Mode) -> Soft<Vec<Field>> {
        if self.eat("(") {
            let mut out = Vec::new();
            loop {
                self.any_ws();
                let at = self.i;
                let Some(f) = self.field(Mode::Top)? else { return self.err(at, "expected a field in the list") };
                out.push(f);
                self.any_ws();
                if self.eat(",") {
                    continue;
                }
                if self.eat(")") {
                    return Ok(Some(out));
                }
                return self.err(self.i, "expected , or ) in the list");
            }
        }
        Ok(self.field(mode)?.map(|f| vec![f]))
    }

    // ------------------------------------------------------------ fields

    fn field(&mut self, mode: Mode) -> Soft<Field> {
        let start = self.i;
        if self.eat("[") {
            let Some(end) = self.rest().find(']') else { return self.err(start, "the [ is never closed") };
            let name = self.rest()[..end].trim().to_string();
            self.i += end + 1;
            if name.is_empty() {
                return self.err(start, "an empty [] is not a name");
            }
            return Ok(Some(Field { name, kind: self.kind()? }));
        }
        if self.eat("\"") {
            let mut name = String::new();
            loop {
                match self.peek() {
                    None => return self.err(start, "the \" is never closed"),
                    Some('"') if self.at("\"\"") => {
                        name.push('"');
                        self.i += 2;
                    }
                    Some('"') => {
                        self.i += 1;
                        break;
                    }
                    Some(c) => {
                        name.push(c);
                        self.i += c.len_utf8();
                    }
                }
            }
            let name = name.trim().to_string();
            if name.is_empty() {
                return self.err(start, "an empty \"\" is not a name");
            }
            return Ok(Some(Field { name, kind: self.kind()? }));
        }
        let mut words: Vec<&str> = Vec::new();
        let mut end = self.i;
        loop {
            let save = self.i;
            if !words.is_empty() && !self.sp() {
                break;
            }
            let Some(w) = self.peek_word() else {
                self.i = save;
                break;
            };
            if self.stops_name(mode, w) {
                self.i = save;
                break;
            }
            self.i += w.len();
            words.push(w);
            end = self.i;
        }
        if words.is_empty() {
            self.i = start;
            return Ok(None);
        }
        self.last_bare = Some((start, end));
        Ok(Some(Field { name: words.join(" "), kind: self.kind()? }))
    }

    fn stops_name(&self, mode: Mode, w: &str) -> bool {
        if RESERVED.iter().any(|r| w.eq_ignore_ascii_case(r)) {
            return true;
        }
        match mode {
            Mode::Top => false,
            Mode::Args => w.eq_ignore_ascii_case("by") || (w.eq_ignore_ascii_case("order") && self.order_by_follows()),
            Mode::Tail => ["for", "left", "inner", "dedupe"].iter().any(|k| w.eq_ignore_ascii_case(k)),
        }
    }

    /// Whether the cursor is at `order` with `by` after it.
    fn order_by_follows(&self) -> bool {
        let mut q = P { s: self.s, i: self.i, depth: self.depth, last_bare: None, pred_start: 0, nest: 0 };
        q.kw("order") && q.sp() && q.at_kw("by")
    }

    fn kind(&mut self) -> Soft<Kind> {
        if self.peek() != Some('@') {
            return Ok(None);
        }
        let at = self.i;
        let rest = &self.rest()[1..];
        let kind = match rest.chars().next().map(|c| c.to_ascii_lowercase()) {
            Some('d') => Kind::Dimension,
            Some('m') => Kind::Measure,
            _ => return self.err(at, "expected @d or @m after the name"),
        };
        if rest[1..].chars().next().is_some_and(is_word_char) {
            return self.err(at, "expected @d or @m after the name");
        }
        self.i += 2;
        Ok(Some(kind))
    }

    // ------------------------------------------------------- projections

    fn projection(&mut self) -> Soft<Projection> {
        let Some(expr) = self.expr(Mode::Top)? else { return Ok(None) };
        let (mut alias, mut order) = (None, None);
        loop {
            let save = self.i;
            if !self.sp() {
                break;
            }
            let at = self.i;
            if self.kw("as") {
                self.ws();
                let at = self.i;
                let Some(f) = self.field(Mode::Top)? else { return self.err(at, "`as` needs an alias after it") };
                if f.kind.is_some() {
                    return self.err(at, "an alias carries no @d or @m");
                }
                if alias.replace(f.name).is_some() {
                    return self.err(at, "the alias is given twice");
                }
            } else if self.kw("asc") || self.kw("desc") {
                let dir = self.s[at..self.i].to_ascii_lowercase();
                if order.replace(dir).is_some() {
                    return self.err(at, "the direction is given twice");
                }
            } else {
                self.i = save;
                break;
            }
        }
        Ok(Some(Projection { expr, alias, order }))
    }

    fn expr(&mut self, mode: Mode) -> Soft<Expr> {
        let start = self.i;
        if let Some(w) = self.peek_word() {
            let func = w.to_ascii_lowercase();
            let save = self.i;
            self.i += w.len();
            self.ws();
            let opens = self.peek() == Some('(');
            if opens {
                match func.as_str() {
                    "calc" => {
                        self.i += 1;
                        let (sql, data_type) = self.sql(start)?;
                        return Ok(Some(Expr::Calc { sql, data_type }));
                    }
                    "extract" => {
                        self.i += 1;
                        self.any_ws();
                        let at = self.i;
                        let part = self.word().map(|p| p.to_ascii_lowercase()).unwrap_or_default();
                        if !PARTS.contains(&part.as_str()) {
                            return self.err(at, format!("extract needs a part first: one of {}", PARTS.join(", ")));
                        }
                        self.any_ws();
                        if !self.eat(",") {
                            return self.err(self.i, "extract(part, field): expected a comma after the part");
                        }
                        self.any_ws();
                        let inner = self.inner_expr("extract")?;
                        self.close(start, "extract")?;
                        return Ok(Some(Expr::Extract { part, inner: Box::new(inner) }));
                    }
                    "custom" => {
                        self.i += 1;
                        self.any_ws();
                        if self.peek() != Some('\'') {
                            return self.err(self.i, "custom('sql with @expression', field): the SQL comes first, single-quoted");
                        }
                        let sql = self.string()?;
                        self.any_ws();
                        if !self.eat(",") {
                            return self.err(self.i, "custom('sql', field): expected a comma after the SQL");
                        }
                        self.any_ws();
                        let inner = self.inner_expr("custom")?;
                        self.close(start, "custom")?;
                        return Ok(Some(Expr::Custom { sql, inner: Box::new(inner) }));
                    }
                    _ if decorator_family(&func).is_some() => {
                        self.i += 1;
                        self.any_ws();
                        let inner = self.inner_expr(&func)?;
                        let (n, by, order) = self.args(&func)?;
                        self.close(start, &func)?;
                        return Ok(Some(Expr::Deco { func, inner: Box::new(inner), n, by, order }));
                    }
                    _ => {}
                }
            }
            self.i = save;
        }
        Ok(self.field(mode)?.map(Expr::Ref))
    }

    fn inner_expr(&mut self, func: &str) -> R<Expr> {
        let at = self.i;
        self.enter(at)?;
        let inner = self.expr(Mode::Args);
        self.nest -= 1;
        match inner? {
            Some(e) => Ok(e),
            None => self.err(at, format!("{func}() needs a field inside it")),
        }
    }

    /// One level deeper, or the refusal that keeps the stack finite.
    fn enter(&mut self, at: usize) -> R<()> {
        self.nest += 1;
        if self.nest > MAX_NEST {
            return self.err(at, format!("nested deeper than {MAX_NEST} levels"));
        }
        Ok(())
    }

    /// The closing parenthesis of a function, with the messages for what
    /// else could be there.
    fn close(&mut self, opened_at: usize, func: &str) -> R<()> {
        self.any_ws();
        if self.eat(")") {
            return Ok(());
        }
        if self.at_kw("by") {
            return self.err(self.i, "a name holding `by` is bracketed inside a function");
        }
        if self.eof() {
            return self.err(opened_at, format!("{func}( is never closed"));
        }
        if self.peek() == Some(',') {
            return self.err(self.i, format!("{func}() takes one field; a number, `by` or `order by` may follow it"));
        }
        self.err(self.i, format!("expected ) to close {func}("))
    }

    /// `, n`, `by a, b` and `order by c desc` after a decorator's field,
    /// checked against what the function takes.
    fn args(&mut self, func: &str) -> R<(Option<i64>, Vec<Field>, Vec<(Field, String)>)> {
        let family = decorator_family(func).unwrap_or("");
        let mut n = None;
        let save = self.i;
        self.any_ws();
        if self.eat(",") {
            self.any_ws();
            let at = self.i;
            match self.int() {
                Some(v) => n = Some(v),
                None => return self.err(at, format!("after the comma in {func}(...), expected a number")),
            }
        } else {
            self.i = save;
        }
        let mut by = Vec::new();
        let save = self.i;
        self.any_ws();
        if self.kw("by") {
            self.ws();
            loop {
                let at = self.i;
                let Some(f) = self.field(Mode::Args)? else { return self.err(at, "`by` needs a projected dimension after it") };
                by.push(f);
                let save = self.i;
                self.any_ws();
                if self.eat(",") {
                    self.any_ws();
                    continue;
                }
                self.i = save;
                break;
            }
        } else {
            self.i = save;
        }
        let mut order = Vec::new();
        let save = self.i;
        self.any_ws();
        if self.kw("order") {
            if !(self.sp() && self.kw("by")) {
                return self.err(self.i, "`order` is followed by `by` and then the dimension");
            }
            self.ws();
            loop {
                let at = self.i;
                let Some(f) = self.field(Mode::Args)? else { return self.err(at, "`order by` needs a projected dimension after it") };
                let mut dir = "asc".to_string();
                let s = self.i;
                if self.sp() && (self.at_kw("asc") || self.at_kw("desc")) {
                    dir = self.word().unwrap().to_ascii_lowercase();
                } else {
                    self.i = s;
                }
                order.push((f, dir));
                let save = self.i;
                self.any_ws();
                if self.eat(",") {
                    self.any_ws();
                    continue;
                }
                self.i = save;
                break;
            }
        } else {
            self.i = save;
        }
        // What each family takes.
        let at = save;
        let moving = func.starts_with("moving_");
        let sized = matches!(func, "ntile" | "lag" | "lead");
        match n {
            Some(_) if !(moving || sized) => return self.err(at, format!("{func}() takes no number")),
            Some(v) if moving && v < 2 => return self.err(at, "a moving frame holds at least two rows: the current one and one before"),
            Some(v) if sized && v < 1 => return self.err(at, format!("{func}() needs a number of at least 1")),
            None if sized => {
                let what = if func == "ntile" { "buckets" } else { "rows" };
                return self.err(at, format!("{func}(field, n) needs the number of {what}"));
            }
            _ => {}
        }
        if !by.is_empty() && !matches!(family, "window" | "contribute") {
            return self.err(at, format!("{func}() takes no `by`; only windows and share do"));
        }
        if !order.is_empty() && family != "window" {
            return self.err(at, format!("{func}() takes no `order by`; only windows do"));
        }
        Ok((n, by, order))
    }

    /// The SQL inside `calc(`: balanced to the parenthesis that closes it,
    /// with an optional `, type` at the top level.
    fn sql(&mut self, opened_at: usize) -> R<(String, Option<String>)> {
        let start = self.i;
        let mut depth = 0usize;
        let mut end;
        loop {
            end = self.i;
            match self.peek() {
                None => return self.err(opened_at, "calc( is never closed"),
                Some('\'') => {
                    self.string()?;
                }
                Some('"') => {
                    self.i += 1;
                    let Some(e) = self.rest().find('"') else { return self.err(end, "the \" is never closed") };
                    self.i += e + 1;
                }
                Some('[') => {
                    self.i += 1;
                    let Some(e) = self.rest().find(']') else { return self.err(end, "the [ is never closed") };
                    self.i += e + 1;
                }
                Some('(') => {
                    depth += 1;
                    self.i += 1;
                }
                Some(')') if depth > 0 => {
                    depth -= 1;
                    self.i += 1;
                }
                Some(')') => break,
                Some(',') if depth == 0 => break,
                Some(c) => self.i += c.len_utf8(),
            }
        }
        let sql = self.s[start..end].trim().to_string();
        if sql.is_empty() {
            return self.err(start, "calc() needs SQL inside it");
        }
        let mut data_type = None;
        if self.eat(",") {
            self.any_ws();
            let at = self.i;
            let ty = self.word().map(|w| w.to_ascii_lowercase()).unwrap_or_default();
            if !TYPES.contains(&ty.as_str()) {
                return self.err(at, format!("after the comma in calc(...), expected a result type: one of {}", TYPES.join(", ")));
            }
            data_type = Some(ty);
            self.any_ws();
        }
        if !self.eat(")") {
            return self.err(self.i, "expected ) to close calc(");
        }
        Ok((sql, data_type))
    }

    /// A single-quoted string with `''` as the quote inside it.
    fn string(&mut self) -> R<String> {
        let start = self.i;
        self.i += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return self.err(start, "the ' is never closed"),
                Some('\'') if self.at("''") => {
                    out.push('\'');
                    self.i += 2;
                }
                Some('\'') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(c) => {
                    out.push(c);
                    self.i += c.len_utf8();
                }
            }
        }
    }

    /// `{...}` balanced over JSON strings, then parsed.
    fn raw(&mut self) -> R<Map<String, Value>> {
        let start = self.i;
        let mut depth = 0usize;
        loop {
            match self.peek() {
                None => return self.err(start, "the { is never closed"),
                Some('"') => {
                    self.i += 1;
                    loop {
                        match self.peek() {
                            None => return self.err(start, "the { is never closed"),
                            Some('\\') => self.i += 1 + self.rest()[1..].chars().next().map_or(0, char::len_utf8),
                            Some('"') => {
                                self.i += 1;
                                break;
                            }
                            Some(c) => self.i += c.len_utf8(),
                        }
                    }
                }
                Some('{') => {
                    depth += 1;
                    self.i += 1;
                }
                Some('}') => {
                    depth -= 1;
                    self.i += 1;
                    if depth == 0 {
                        break;
                    }
                }
                Some(c) => self.i += c.len_utf8(),
            }
        }
        let text = &self.s[start..self.i];
        match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(m)) => Ok(m),
            Ok(_) => self.err(start, "spec JSON must be an object"),
            Err(e) => {
                let at = if e.line() == 1 { start + e.column().saturating_sub(1) } else { start };
                self.err(at, format!("spec JSON does not parse: {}", e.to_string().split(" at line").next().unwrap_or("")))
            }
        }
    }

    // ----------------------------------------------------------- filters

    fn filter(&mut self) -> Soft<FilterNode> {
        let Some(first) = self.conj()? else { return Ok(None) };
        let mut alts = vec![first];
        loop {
            let save = self.i;
            self.any_ws();
            if self.kw("or") {
                self.ws();
                let at = self.i;
                match self.conj()? {
                    Some(c) => alts.push(c),
                    None => return self.err(at, "after `or`, expected `field predicate value`; a value holding and/or is single-quoted"),
                }
            } else {
                self.i = save;
                break;
            }
        }
        Ok(Some(if alts.len() == 1 { alts.pop().unwrap() } else { FilterNode::Or(alts) }))
    }

    fn conj(&mut self) -> Soft<FilterNode> {
        let Some(first) = self.pred()? else { return Ok(None) };
        let mut preds = vec![first];
        loop {
            let save = self.i;
            self.any_ws();
            if self.kw("and") {
                self.ws();
                let at = self.i;
                match self.pred()? {
                    Some(p) => preds.push(p),
                    None => return self.err(at, "after `and`, expected `field predicate value`; a value holding and/or is single-quoted"),
                }
            } else {
                self.i = save;
                break;
            }
        }
        Ok(Some(if preds.len() == 1 { preds.pop().unwrap() } else { FilterNode::And(preds) }))
    }

    fn pred(&mut self) -> Soft<FilterNode> {
        let start = self.i;
        if self.eat("(") {
            self.enter(start)?;
            self.any_ws();
            let at = self.i;
            let inner = self.filter();
            self.nest -= 1;
            let Some(f) = inner? else { return self.err(at, "expected a filter inside the parentheses") };
            self.any_ws();
            if !self.eat(")") {
                return self.err(self.i, "expected ) to close the filter group");
            }
            return Ok(Some(f));
        }
        self.pred_start = start;
        let Some(field) = self.field(Mode::Top)? else { return Ok(None) };
        self.ws();
        match self.test()? {
            Some(pred) => Ok(Some(FilterNode::Leaf(Leaf { field, pred }))),
            None => {
                self.i = start;
                Ok(None)
            }
        }
    }

    fn test(&mut self) -> Soft<Pred> {
        let at = self.i;
        for (op, name) in [
            (">=", "greater_than_or_equal_to"),
            ("<=", "less_than_or_equal_to"),
            ("!=", "does_not_equal"),
            ("<>", "does_not_equal"),
            ("=", "equals"),
            (">", "greater_than"),
            ("<", "less_than"),
        ] {
            if self.eat(op) {
                if matches!(self.peek(), Some('=' | '<' | '>' | '!')) {
                    return self.err(at, format!("`{op}{}` is not an operator; use =, !=, <>, >, >=, < or <=", self.peek().unwrap()));
                }
                self.ws();
                let value = self.value()?;
                return Ok(Some(Pred::Cmp { op: name, value }));
            }
        }
        let negated = self.kw("not");
        if negated {
            if !self.sp() {
                return self.err(self.i, "`not` goes before in, contains, starts with or ends with");
            }
            if matches!(self.peek(), Some('=' | '<' | '>' | '!')) {
                return self.err(at, "write != instead of not =");
            }
            if self.at_kw("between") {
                return self.err(at, "the spec has no `not between`; write two comparisons");
            }
            if self.peek() == Some('(') {
                return self.err(at, "the spec has no `not (...)`; negate each filter inside");
            }
        }
        if self.kw("in") {
            self.ws();
            if self.peek() != Some('(') {
                return self.err(
                    at,
                    format!(
                        "`in` takes a parenthesised list, as in `x in (a, b)`; a name holding `in` is bracketed: [{}]",
                        self.name_tail(self.pred_start)
                    ),
                );
            }
            return Ok(Some(Pred::In { negated, values: self.list()? }));
        }
        for (kw, which) in [("contains", "contains"), ("starts", "starts_with"), ("ends", "ends_with")] {
            if self.kw(kw) {
                if which != "contains" && !(self.sp() && self.kw("with")) {
                    return self.err(self.i, format!("`{kw}` is followed by `with` and then the value"));
                }
                if !self.sp() {
                    return self.err(self.i, "expected a space and then the value");
                }
                return Ok(Some(Pred::Like { which, negated, value: self.value()? }));
            }
        }
        if negated {
            return self.err(at, "`not` goes before in, contains, starts with or ends with");
        }
        if self.kw("between") {
            if !self.sp() {
                return self.err(self.i, "between needs `a and b`");
            }
            let low = self.value()?;
            if !(self.sp() && self.kw("and") && self.sp()) {
                return self.err(self.i, "between needs `and` and then the upper bound");
            }
            let high = self.value()?;
            return Ok(Some(Pred::Between { low, high }));
        }
        if self.kw("is") {
            self.ws();
            let negated = self.kw("not") && self.sp();
            if !self.kw("null") {
                return self.err(
                    at,
                    format!(
                        "`is` is followed by `null` or `not null`; a name holding `is` is bracketed: [{}]",
                        self.name_tail(self.pred_start)
                    ),
                );
            }
            return Ok(Some(Pred::Null { negated }));
        }
        if self.kw("top") {
            self.ws();
            let Some(n) = self.int() else {
                return self.err(
                    at,
                    format!(
                        "top needs a count, as in `category top 10 by revenue`; a name holding `top` is bracketed: [{}]",
                        self.name_tail(self.pred_start)
                    ),
                );
            };
            if n < 1 {
                return self.err(at, "top needs a count of at least 1");
            }
            let save = self.i;
            let by = if self.sp() && self.kw("by") {
                self.ws();
                let at = self.i;
                let Some(f) = self.field(Mode::Top)? else { return self.err(at, "`top N by` needs a measure after it") };
                Some(f)
            } else {
                self.i = save;
                None
            };
            return Ok(Some(Pred::Top { n, by }));
        }
        if self.kw("keyword") {
            self.ws();
            return Ok(Some(Pred::Keyword { values: self.list()? }));
        }
        Ok(None)
    }

    /// A value: a string, or text to the next separator.
    fn value(&mut self) -> R<Val> {
        match self.peek() {
            Some('\'') => Ok(Val { text: self.string()?, quoted: true }),
            Some('"') => self.err(self.i, "a value is single-quoted; double quotes name a field"),
            _ => {
                let start = self.i;
                let rest = self.rest();
                let mut end = rest.len();
                for (pos, c) in rest.char_indices() {
                    if matches!(c, ',' | ')' | '{' | '\n' | '\r') {
                        end = pos;
                        break;
                    }
                    if c.is_whitespace() {
                        let mut q = P { s: self.s, i: self.i + pos, depth: self.depth, last_bare: None, pred_start: 0, nest: 0 };
                        q.ws();
                        if q.at_kw("and") || q.at_kw("or") {
                            end = pos;
                            break;
                        }
                    }
                }
                let text = rest[..end].trim_end();
                if text.is_empty() {
                    return self.err(start, "expected a value");
                }
                self.i = start + text.len();
                Ok(Val { text: text.to_string(), quoted: false })
            }
        }
    }

    /// `(a, b, c)`: values to the next comma or parenthesis.
    fn list(&mut self) -> R<Vec<Val>> {
        let start = self.i;
        if !self.eat("(") {
            return self.err(start, "expected a parenthesised list, as in `in (a, b)`");
        }
        let mut out = Vec::new();
        loop {
            self.any_ws();
            if self.peek() == Some(')') {
                break;
            }
            let at = self.i;
            let v = match self.peek() {
                Some('\'') => Val { text: self.string()?, quoted: true },
                Some('"') => return self.err(at, "a value is single-quoted; double quotes name a field"),
                _ => {
                    let rest = self.rest();
                    let end = rest.find([',', ')', '\n', '\r']).unwrap_or(rest.len());
                    let text = rest[..end].trim_end().to_string();
                    if text.is_empty() {
                        return self.err(at, "expected a value in the list");
                    }
                    self.i += text.len();
                    Val { text, quoted: false }
                }
            };
            if v.text.contains(',') {
                return self
                    .err(at, "a list value cannot hold a comma: the spec joins the list with commas and the planner splits it again");
            }
            out.push(v);
            self.any_ws();
            if self.eat(",") {
                continue;
            }
            if self.peek() == Some(')') {
                break;
            }
            if self.eof() {
                return self.err(start, "the ( of the list is never closed");
            }
            return self.err(self.i, "expected , or ) in the list");
        }
        self.i += 1;
        if out.is_empty() {
            return self.err(start, "the list needs at least one value");
        }
        Ok(out)
    }

    // ---------------------------------------------------------- segments

    fn segment(&mut self) -> Soft<ItemKind> {
        let save = self.i;
        let kind = if self.kw("include") {
            SegmentKind::Include
        } else if self.kw("exclude") {
            SegmentKind::Exclude
        } else if self.kw("expand") {
            SegmentKind::Expand
        } else {
            return Ok(None);
        };
        self.any_ws();
        if !self.eat("(") {
            self.i = save;
            return Ok(None);
        }
        let opened_at = self.i - 1;
        self.depth += 1;
        let mut items = Vec::new();
        loop {
            self.any_ws();
            if self.eat(")") {
                break;
            }
            if self.eof() {
                return self.err(opened_at, "the segment's ( is never closed");
            }
            let at = self.i;
            if let Some(f) = self.filter()? {
                items.push(SegmentItem::Filter(f));
            } else if let Some(f) = self.field(Mode::Top)? {
                items.push(match f.kind {
                    Some(Kind::Measure) => SegmentItem::Measure(f),
                    _ => SegmentItem::Key(f),
                });
            } else {
                return self.err(at, "a segment holds fields and filters: a lone name is a key, with @m a measure");
            }
            self.ws();
            match self.peek() {
                Some(',') | Some('\n') | Some('\r') => self.i += 1,
                Some(')') => {}
                None => return self.err(opened_at, "the segment's ( is never closed"),
                _ => {
                    let last = Item { kind: ItemKind::Limit(0), raw: None, text: self.s[at..self.i].trim().to_string() };
                    return Err(self.item_end_error(&last));
                }
            }
        }
        self.depth -= 1;
        if items.is_empty() {
            return self.err(opened_at, "a segment needs at least a key dimension");
        }
        let mut carried = Vec::new();
        if kind == SegmentKind::Expand {
            self.any_ws();
            if !self.kw("with") {
                return self.err(self.i, "expand (...) is followed by with (...) naming the dimensions it carries");
            }
            self.any_ws();
            if !self.eat("(") {
                return self.err(self.i, "with needs a parenthesised list of dimensions");
            }
            loop {
                self.any_ws();
                let at = self.i;
                let Some(f) = self.field(Mode::Top)? else { return self.err(at, "expected a dimension to carry") };
                let s = self.i;
                let alias = if self.sp() && self.kw("as") && self.sp() {
                    let at = self.i;
                    let Some(a) = self.field(Mode::Top)? else { return self.err(at, "`as` needs a column name after it") };
                    Some(a.name)
                } else {
                    self.i = s;
                    None
                };
                carried.push((f, alias));
                self.any_ws();
                if self.eat(",") {
                    continue;
                }
                if self.eat(")") {
                    break;
                }
                return self.err(self.i, "expected , or ) in the with list");
            }
        }
        let (mut name, mut apply_to, mut join, mut dedupe) = (None, Vec::new(), None, None);
        loop {
            let save = self.i;
            if !self.sp() {
                break;
            }
            let at = self.i;
            if self.kw("as") {
                self.ws();
                let at = self.i;
                let Some(f) = self.field(Mode::Tail)? else { return self.err(at, "`as` needs the segment's name after it") };
                if name.replace(f.name).is_some() {
                    return self.err(at, "the segment's name is given twice");
                }
            } else if self.kw("for") {
                if kind == SegmentKind::Expand {
                    return self.err(at, "an expanding segment constrains the whole query; `for` does not apply");
                }
                self.ws();
                let at = self.i;
                let Some(names) = self.names(Mode::Tail)? else { return self.err(at, "`for` needs a measure or a list of them") };
                apply_to.extend(names);
            } else if self.kw("left") || self.kw("inner") {
                if kind != SegmentKind::Expand {
                    return self.err(at, "`left` and `inner` apply to expand (...)");
                }
                if join.replace(self.s[at..self.i].to_ascii_lowercase()).is_some() {
                    return self.err(at, "the join is given twice");
                }
            } else if self.kw("dedupe") {
                if kind != SegmentKind::Expand {
                    return self.err(at, "`dedupe` applies to expand (...)");
                }
                if !self.sp() {
                    return self.err(self.i, "dedupe takes neq, lt or none");
                }
                let at = self.i;
                let w = self.word().map(|w| w.to_ascii_lowercase()).unwrap_or_default();
                if !["neq", "lt", "none"].contains(&w.as_str()) {
                    return self.err(at, "dedupe takes neq, lt or none");
                }
                if dedupe.replace(w).is_some() {
                    return self.err(at, "dedupe is given twice");
                }
            } else {
                self.i = save;
                break;
            }
        }
        Ok(Some(ItemKind::Segment(Segment { kind, items, carried, name, apply_to, join, dedupe })))
    }
}

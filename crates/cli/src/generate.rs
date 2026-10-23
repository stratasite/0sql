//! `zsql generate`: model files from a catalog. A draft for a person,
//! never a replacement for one: an existing file is never rewritten, every
//! guess is marked on its line, and what could not be mapped is listed
//! rather than silently left out. See `docs/introspection.md`, phase C.

use crate::{catalog, config, templates, tty};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use zsql_source::{Catalog, Column, Connection, Table};

pub struct Options<'a> {
    pub datasource: Option<&'a str>,
    /// Tables to draft, as schema.table or table; none means the picker.
    pub tables: &'a [String],
    /// The `models/<dir>` the files go in. Without one, each table goes
    /// under its own schema, which is a reasonable default and not a
    /// rule: the directories are the model's subject areas, and need not
    /// mirror the warehouse.
    pub dir: Option<&'a str>,
    pub schema: Option<&'a str>,
    /// Every table in the catalog: never the default.
    pub all: bool,
    pub columns: &'a [String],
    pub new_columns: bool,
    /// A named table the catalog lacks is read from the warehouse first.
    pub live: bool,
    pub dry_run: bool,
}

/// Column names that are a person, a contact or a credential by their
/// name alone: short strings, but not categories, and not for a sample
/// file that travels with the model. The width rule still holds; the
/// line is written commented out with the reason, for the author to
/// turn on when the column is in fact a category.
const IDENTITY_NAMES: [&str; 22] = [
    "name",
    "first_name",
    "last_name",
    "full_name",
    "email",
    "mail",
    "phone",
    "mobile",
    "address",
    "street",
    "zip",
    "postcode",
    "postal_code",
    "ssn",
    "dob",
    "birth",
    "ip",
    "user",
    "login",
    "password",
    "token",
    "secret",
];

fn looks_like_identity(column: &str) -> bool {
    let c = column.to_lowercase();
    IDENTITY_NAMES.iter().any(|n| c == *n || c.ends_with(&format!("_{n}")) || c.starts_with(&format!("{n}_")))
}

/// What a warehouse type becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mapped {
    Known(&'static str),
    /// Text-shaped but not text: a uuid, a json document, an enum. A
    /// string, and said so on the line.
    Text,
    /// Nothing 0sql has a type for: an array, an interval, a geometry.
    Skip,
}

/// The warehouse's spelling, lower-cased, parameters and ClickHouse's
/// wrappers stripped: `character varying(80)` is `character varying`,
/// `Nullable(LowCardinality(String))` is `string`.
fn normalize(data_type: &str) -> (String, bool) {
    let mut t = data_type.trim().to_lowercase();
    let mut array = t.ends_with("[]") || t.starts_with("array(") || t.starts_with("array<");
    for wrapper in ["nullable(", "lowcardinality("] {
        while let Some(rest) = t.strip_prefix(wrapper) {
            t = rest.trim_end_matches(')').to_string();
        }
    }
    if let Some(i) = t.find('(') {
        t.truncate(i);
    }
    if let Some(i) = t.find('<') {
        t.truncate(i);
    }
    t = t.trim().trim_end_matches("[]").to_string();
    if t == "array" {
        array = true;
    }
    (t, array)
}

pub(crate) fn map_type(data_type: &str) -> Mapped {
    let (t, array) = normalize(data_type);
    if array {
        return Mapped::Skip;
    }
    use Mapped::*;
    match t.as_str() {
        "char" | "character" | "varchar" | "character varying" | "text" | "nvarchar" | "nchar" | "ntext" | "string" | "name" | "citext"
        | "bpchar" | "tinytext" | "mediumtext" | "longtext" | "clob" | "varchar2" | "fixedstring" => Known("string"),
        "uuid" | "uniqueidentifier" | "json" | "jsonb" | "xml" | "enum" | "enum8" | "enum16" | "set" | "inet" | "cidr" | "macaddr"
        | "macaddr8" | "ipv4" | "ipv6" | "variant" | "object" | "sql_variant" => Text,
        "int" | "integer" | "int4" | "smallint" | "int2" | "tinyint" | "mediumint" | "int8" | "int16" | "int32" | "uint8" | "uint16"
        | "serial" | "smallserial" => Known("integer"),
        "bigint" | "int64" | "uint32" | "uint64" | "bigserial" | "unsigned big int" => Known("bigint"),
        "numeric" | "decimal" | "money" | "smallmoney" | "real" | "float" | "float4" | "float8" | "double" | "double precision"
        | "decimal32" | "decimal64" | "decimal128" | "decimal256" | "float32" | "float64" | "number" => Known("decimal"),
        "date" | "date32" => Known("date"),
        "timestamp"
        | "timestamp without time zone"
        | "timestamp with time zone"
        | "timestamptz"
        | "datetime"
        | "datetime2"
        | "datetimeoffset"
        | "smalldatetime"
        | "datetime64" => Known("date_time"),
        "boolean" | "bool" | "bit" => Known("boolean"),
        "bytea" | "blob" | "binary" | "varbinary" | "image" | "bytes" | "longblob" | "mediumblob" | "tinyblob" => Known("binary"),
        _ => Skip,
    }
}

/// `customer_id` → `Customer Id`, `orderDate` → `Order Date`.
fn title(name: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c == '_' || *c == '-' || *c == ' ' {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
        } else if c.is_uppercase() && i > 0 && chars[i - 1].is_lowercase() {
            words.push(std::mem::take(&mut cur));
            cur.push(*c);
        } else {
            cur.push(*c);
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
        .iter()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

const ADDITIVE: [&str; 20] = [
    "total", "amount", "amt", "qty", "quantity", "price", "revenue", "cost", "sales", "count", "cnt", "sum", "value", "balance", "fee",
    "tax", "discount", "units", "weight", "volume",
];

/// Whether a numeric column looks like something to add up. Keys never
/// are, whatever they are called.
fn looks_additive(column: &Column, table: &Table) -> bool {
    let n = column.name.to_lowercase();
    if n == "id" || n.ends_with("_id") || n.ends_with("id") && n.len() <= 4 {
        return false;
    }
    if table.primary_key.iter().any(|k| k.eq_ignore_ascii_case(&column.name)) {
        return false;
    }
    if table.foreign_keys.iter().any(|fk| fk.columns.iter().any(|k| k.eq_ignore_ascii_case(&column.name))) {
        return false;
    }
    ADDITIVE.iter().any(|w| n == *w || n.starts_with(&format!("{w}_")) || n.ends_with(&format!("_{w}")) || n.contains(&format!("_{w}_")))
}

/// The pruner's cost, from the row estimate: dimension tables low, big
/// facts high, as the table template says.
fn cost(rows: Option<i64>) -> (i64, String) {
    match rows {
        None => (10, "no row estimate from the warehouse".into()),
        Some(n) if n < 10_000 => (1, format!("about {n} rows")),
        Some(n) if n < 1_000_000 => (10, format!("about {} thousand rows", n / 1_000)),
        Some(n) if n < 100_000_000 => (100, format!("about {} million rows", n / 1_000_000)),
        Some(n) => (1_000, format!("about {} million rows", n / 1_000_000)),
    }
}

/// A YAML scalar, quoted when YAML would otherwise read it as something
/// else or choke on it.
fn yaml(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| c.is_alphanumeric() || " _-./()".contains(c))
        && !s.contains(": ")
        && !s.starts_with(['-', '?', '*', '&', '!', '|', '>', '\'', '"', '%', '@', '`', '#', ' '])
        && !matches!(s.to_lowercase().as_str(), "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "~")
        && s.parse::<f64>().is_err();
    if plain {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// What the warehouse needs of a column name in an expression: the words
/// it reads as syntax rather than as a name, and how it quotes a name.
pub(crate) struct Lists {
    reserved: Vec<String>,
    quote: (String, String),
}

impl Lists {
    pub(crate) fn of(adapter: &str) -> Lists {
        let dialect = crate::local::dialects().get(adapter).ok();
        let reserved = dialect.as_ref().map(|d| d.reserved_keywords()).unwrap_or_default();
        let quote = dialect
            .as_ref()
            .and_then(|d| d.get("quote").and_then(|v| v.as_str()).map(zsql_spec::expr::quote_pair))
            .unwrap_or_else(|| ("\"".into(), "\"".into()));
        Lists { reserved, quote }
    }

    /// A column as an expression reads it: bare when the warehouse would
    /// take it for a name, quoted when it is a reserved word or holds
    /// anything a bare name may not. A dimension whose expression is a
    /// reserved word parses to no column at all, and the loader refuses
    /// it, which is what `DAY` on a transactions table used to do.
    pub(crate) fn column(&self, name: &str) -> String {
        let mut chars = name.chars();
        let plain = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
            && !self.reserved.iter().any(|r| r.eq_ignore_ascii_case(name));
        if plain {
            name.to_string()
        } else {
            let (open, close) = (&self.quote.0, &self.quote.1);
            format!("{open}{}{close}", name.replace(close.as_str(), &format!("{close}{close}")))
        }
    }
}

/// The data type each field name is declared with, reconciled over every
/// table of this run and whatever the models already say.
///
/// A field is one field in the model however many tables map it, which is
/// how a conformed dimension works: `Week No` on the fact and on the date
/// table is one field with two expressions. The loader keeps the first
/// declaration it reads and drops the rest, so two drafts that disagree
/// about a column's type would leave whichever file loaded first in
/// charge, and `check` would report the other table as a mismatch. So the
/// type is decided once here, for every table at once: the widest of what
/// the warehouse says, with a note when a widening or a disagreement
/// happened.
pub(crate) type Declared = BTreeMap<String, (String, Option<String>)>;

/// The wider of two 0sql types, where one is wider; `None` when they are
/// different kinds of thing and no warehouse type reconciles them.
/// The widest of what a field's columns hold, where one is widest; a
/// field mapped to a TINYINT and a BIGINT should declare bigint, which
/// covers both. `None` when the types are different kinds of thing and
/// no declaration describes them all.
pub(crate) fn widest<'a>(types: impl IntoIterator<Item = &'a str>) -> Option<&'static str> {
    let mut it = types.into_iter();
    let first = it.next()?;
    let mut held: &'static str = widen(first, first).unwrap_or_else(|| name_of(first));
    for next in it {
        if next == held {
            continue;
        }
        held = widen(held, next)?;
    }
    Some(held)
}

/// A type name as the static str the tables hold.
fn name_of(t: &str) -> &'static str {
    const ALL: [&str; 8] = ["string", "integer", "bigint", "decimal", "date", "date_time", "boolean", "binary"];
    ALL.iter().find(|x| **x == t).copied().unwrap_or("")
}

pub(crate) fn widen(a: &str, b: &str) -> Option<&'static str> {
    const FAMILIES: [&[&str]; 2] = [&["integer", "bigint", "decimal"], &["date", "date_time"]];
    FAMILIES.iter().find_map(|fam| {
        let (i, j) = (fam.iter().position(|x| *x == a)?, fam.iter().position(|x| *x == b)?);
        Some(fam[i.max(j)])
    })
}

fn declared_types(tables: &[&Table], have: &[Existing]) -> Declared {
    let mut out: Declared = BTreeMap::new();
    // What the models already declare comes first: a new draft agrees
    // with the files rather than contradicting them.
    for e in have {
        for (name, data_type) in &e.declared {
            out.entry(name.to_lowercase()).or_insert((data_type.clone(), None));
        }
    }
    for t in tables {
        for c in &t.columns {
            let fresh = match map_type(&c.data_type) {
                Mapped::Known(x) => x.to_string(),
                Mapped::Text => "string".to_string(),
                Mapped::Skip => continue,
            };
            let key = title(&c.name).to_lowercase();
            match out.get(&key) {
                None => {
                    out.insert(key, (fresh, None));
                }
                Some((held, _)) if *held == fresh => {}
                Some((held, _)) => match widen(held, &fresh) {
                    Some(wider) => {
                        let note = format!("one field across tables, so the wider of {held} and {fresh}");
                        out.insert(key, (wider.to_string(), Some(note)));
                    }
                    None => {
                        let note = format!("one field across tables, and another declares it {fresh}: one of the two is wrong");
                        let held = held.clone();
                        out.insert(key, (held, Some(note)));
                    }
                },
            }
        }
    }
    out
}

/// One field's lines, or why it has none.
fn field(column: &Column, table: &Table, lists: &Lists, declared: &Declared) -> std::result::Result<String, String> {
    let (mapped, mut note) = match map_type(&column.data_type) {
        Mapped::Known(t) => (t.to_string(), column.data_type.clone()),
        Mapped::Text => ("string".to_string(), format!("{}; inferred: text-shaped, so a string", column.data_type)),
        Mapped::Skip => return Err(format!("{}: {} has no 0sql type", column.name, column.data_type)),
    };
    // The field's type is the model's, not this table's: see `Declared`.
    let data_type = match declared.get(&title(&column.name).to_lowercase()) {
        Some((t, why)) => {
            if *t != mapped {
                note = match why {
                    Some(w) => format!("{} here; {w}", column.data_type),
                    None => format!("{} here; the model declares this field {t}", column.data_type),
                };
            } else if let Some(w) = why {
                note = format!("{}; {w}", column.data_type);
            }
            t.clone()
        }
        None => mapped,
    };
    let data_type = data_type.as_str();
    let numeric = matches!(data_type, "integer" | "bigint" | "decimal");
    let measure = numeric && looks_additive(column, table);
    let mut out = String::new();
    if measure {
        out.push_str(&format!("{:<32}# inferred: \"{}\" looks additive; make it a dimension if not\n", "  - type: measure", column.name));
    } else {
        out.push_str("  - type: dimension\n");
    }
    out.push_str(&format!("    name: {}\n", yaml(&title(&column.name))));
    if let Some(c) = &column.comment {
        out.push_str(&format!("    description: {}\n", yaml(c)));
    }
    out.push_str(&format!("{:<32}# {}\n", format!("    data_type: {data_type}"), note));
    if data_type == "string" && !measure {
        if let Some(n) = column.max_length.filter(|n| (1..=255).contains(n)) {
            let key = table.primary_key.iter().any(|k| k.eq_ignore_ascii_case(&column.name))
                || table.foreign_keys.iter().any(|f| f.columns.iter().any(|k| k.eq_ignore_ascii_case(&column.name)));
            if key {
                out.push_str(&format!(
                    "{:<32}# a key, {n} wide: values nobody names; turn on only if these are categories\n",
                    "    # enable_sampling: true"
                ));
            } else if looks_like_identity(&column.name) {
                out.push_str(&format!(
                    "{:<32}# inferred: {n} wide, but looks like a person or a credential; turn on only if these are categories\n",
                    "    # enable_sampling: true"
                ));
            } else {
                out.push_str(&format!("{:<32}# inferred: a short string, {n} wide\n", "    enable_sampling: true"));
            }
        }
    }
    out.push_str("    expression:\n");
    let sql = lists.column(&column.name);
    if measure {
        out.push_str(&format!("      sql: sum({sql})\n"));
    } else {
        out.push_str(&format!("      sql: {}\n", yaml(&sql)));
        if table.primary_key.len() == 1 && table.primary_key[0].eq_ignore_ascii_case(&column.name) {
            out.push_str("      primary_key: true\n");
        }
    }
    Ok(out)
}

struct Drafted {
    text: String,
    fields: usize,
    measures: usize,
    skipped: Vec<String>,
}

fn table_yml(cat: &Catalog, table: &Table, today: &str, lists: &Lists, declared: &Declared) -> Drafted {
    let (cost, why) = cost(table.rows);
    let mut body = String::new();
    let (mut fields, mut measures, mut skipped) = (0, 0, Vec::new());
    for column in &table.columns {
        match field(column, table, lists, declared) {
            Ok(lines) => {
                fields += 1;
                if lines.starts_with("  - type: measure") {
                    measures += 1;
                }
                body.push_str(&lines);
            }
            Err(why) => skipped.push(why),
        }
    }
    let mut text = format!(
        "# Generated from catalog/{}.json on {}. Every line ending in \"inferred\"\n\
         # is a guess from a column's name or type: check it. This file is yours\n\
         # from here; `zsql generate` never rewrites it, and lists new columns.\n",
        cat.datasource, today
    );
    if !skipped.is_empty() {
        text.push_str("# Not generated, since 0sql has no type for them:\n");
        for s in &skipped {
            text.push_str(&format!("#   {s}\n"));
        }
    }
    text.push_str(&format!(
        "name: {}\nphysical_name: {}\ndatasource: {}\n",
        yaml(&title(&table.name)),
        yaml(&table.physical_name()),
        cat.datasource
    ));
    if let Some(c) = &table.comment {
        text.push_str(&format!("description: {}\n", yaml(c)));
    }
    text.push_str(&format!("{:<32}# inferred: {}\n", format!("cost: {cost}"), why));
    text.push_str("\nfields:\n");
    text.push_str(&body);
    Drafted { text, fields, measures, skipped }
}

/// The joins a schema's foreign keys declare, in the shape of a rel file.
fn relation_yml(cat: &Catalog, tables: &[&Table], today: &str, lists: &Lists) -> Option<String> {
    let mut joins = String::new();
    for t in tables {
        for fk in &t.foreign_keys {
            let Some(right) = cat.table(fk.references_schema.as_deref().or(t.schema.as_deref()), &fk.references_table) else {
                continue;
            };
            let on: Vec<String> = fk
                .columns
                .iter()
                .zip(&fk.references_columns)
                .map(|(l, r)| format!("left.{} = right.{}", lists.column(l), lists.column(r)))
                .collect();
            let key = format!("{}_{}", t.name, right.name).to_lowercase();
            // The model holds one join per pair of tables. A second key to
            // the same table (bill_to and ship_to, say) wants a second
            // model of that table, a role, which is the author's to name;
            // it is written down here, commented out, so it is not lost.
            if joins.contains(&format!("\n{key}:")) || joins.starts_with(&format!("{key}:")) {
                joins.push_str(&format!(
                    "# {} also references {} through {}: one join per pair of tables, so this one needs\n\
                     # a second model of {} (a role, with its own physical_name) before it can be a join.\n\
                     #   sql: {}\n",
                    t.name,
                    right.name,
                    fk.columns.join(", "),
                    right.name,
                    on.join(" AND ")
                ));
                continue;
            }
            joins.push_str(&format!(
                "{key}:\n  left: {}\n  right: {}\n  sql: {}\n{:<32}# inferred: a foreign key\n  join: inner\n",
                yaml(&title(&t.name)),
                yaml(&title(&right.name)),
                yaml(&on.join(" AND ")),
                "  cardinality: many_to_one"
            ));
        }
    }
    if joins.is_empty() {
        return None;
    }
    Some(format!(
        "# Generated from catalog/{}.json on {}: one join per foreign key the\n\
         # warehouse declares. Yours from here; `zsql generate` never rewrites it.\n\
         datasource: {}\n\n{joins}",
        cat.datasource, today, cat.datasource
    ))
}

/// Every model table file in the project: its path, its physical name, and
/// the column names its field expressions use.
struct Existing {
    path: PathBuf,
    /// The physical name's last two parts, unquoted and lower-cased.
    schema: Option<String>,
    table: String,
    datasource: Option<String>,
    columns: BTreeSet<String>,
    /// (field name, data_type) as this file declares them, so a new
    /// draft of another table agrees with what the model already says.
    declared: Vec<(String, String)>,
}

fn existing(project_dir: &Path) -> Result<Vec<Existing>> {
    let mut out = Vec::new();
    let models = project_dir.join("models");
    if !models.is_dir() {
        return Ok(out);
    }
    let mut stack = vec![models];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !(name.starts_with("tbl.") && (name.ends_with(".yml") || name.ends_with(".yaml"))) {
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(&text) else { continue };
            let Some(physical) = v.get("physical_name").and_then(|p| p.as_str()) else { continue };
            let datasource = v.get("datasource").and_then(|d| d.as_str()).map(|d| d.trim().to_lowercase());
            // Any quoting the dialect allows is stripped: what remains is
            // compared without case, as the catalog's own names are.
            let bare = |s: &str| s.trim().trim_matches(|c| matches!(c, '"' | '`' | '[' | ']')).to_lowercase();
            let (schema, table) = crate::catalog_check::parts(physical, &("\"".into(), "\"".into()));
            let (schema, table) = (schema.map(bare), bare(table));
            let mut columns = BTreeSet::new();
            let mut declared = Vec::new();
            if let Some(fields) = v.get("fields").and_then(|f| f.as_sequence()) {
                for f in fields {
                    if let (Some(n), Some(d)) = (f.get("name").and_then(|x| x.as_str()), f.get("data_type").and_then(|x| x.as_str())) {
                        declared.push((n.trim().to_string(), d.trim().to_lowercase()));
                    }
                    let expr = match f.get("expression") {
                        Some(serde_yaml::Value::String(s)) => Some(s.clone()),
                        Some(m) => m.get("sql").and_then(|s| s.as_str()).map(str::to_string),
                        None => None,
                    };
                    if let Some(e) = expr {
                        columns.extend(identifiers(&e));
                    }
                }
            }
            out.push(Existing { path, schema, table, datasource, columns, declared });
        }
    }
    Ok(out)
}

/// The bare identifiers in an expression, lower-cased, less the words
/// that are SQL rather than columns.
fn identifiers(sql: &str) -> Vec<String> {
    const WORDS: [&str; 22] = [
        "sum", "count", "avg", "min", "max", "coalesce", "cast", "as", "and", "or", "not", "case", "when", "then", "else", "end", "null",
        "distinct", "in", "is", "like", "between",
    ];
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in sql.chars().chain(std::iter::once(' ')) {
        if c.is_alphanumeric() || c == '_' {
            cur.push(c);
        } else {
            if !cur.is_empty() && !cur.chars().next().unwrap().is_ascii_digit() {
                let w = cur.to_lowercase();
                if !WORDS.contains(&w.as_str()) {
                    out.push(w);
                }
            }
            cur.clear();
        }
    }
    out
}

/// Appends field entries to a model file, which is safe only when its
/// `fields:` list is the last top-level key; otherwise the caller prints
/// them to paste.
fn append_fields(path: &Path, entries: &str) -> Result<bool> {
    let text = std::fs::read_to_string(path)?;
    // The last top-level key must be fields:, so entries land in its list.
    let top: Vec<&str> = text.lines().filter(|l| !l.starts_with([' ', '\t', '#']) && l.contains(':') && !l.starts_with('-')).collect();
    if top.last().map(|l| l.trim_end()) != Some("fields:") {
        return Ok(false);
    }
    // The list's own indent: a file may put its items at the margin
    // (`fields:\n- type:`) or two in, and an entry must match or the
    // file stops loading. The generated entries are written two in.
    let indent = text
        .lines()
        .skip_while(|l| l.trim_end() != "fields:")
        .skip(1)
        .find(|l| l.trim_start().starts_with("- "))
        .map(|l| l.len() - l.trim_start().len())
        .unwrap_or(2);
    let mut out = text.clone();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    for line in entries.lines() {
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        let bare = line.strip_prefix("  ").unwrap_or(line);
        out.push_str(&" ".repeat(indent));
        out.push_str(bare);
        out.push('\n');
    }
    std::fs::write(path, out)?;
    Ok(true)
}

pub fn run(project_dir: Option<&Path>, opts: Options<'_>) -> Result<()> {
    let project = config::find_project(project_dir.unwrap_or(Path::new(".")))?;
    let all = Connection::read_all(&project.dir).context("reading datasources.yml")?;
    let conn = catalog::choose(&project.dir, opts.datasource)?;
    let uid = conn.uid.clone();
    let _ = all;
    let mut cat = match catalog::load(&project.dir, &uid)? {
        Some(c) => c,
        None => {
            if tty::interactive()
                && tty::confirm(&format!("{uid}: no catalog yet. Read it now?"), "zsql catalog read -d {uid} --schema NAME")?
            {
                crate::catalog::read(Some(&project.dir), Some(&uid), &[], &[], false)?;
                catalog::load(&project.dir, &uid)?.ok_or_else(|| anyhow::anyhow!("{uid}: still no catalog"))?
            } else {
                bail!("{uid}: no catalog yet; `zsql catalog read -d {uid} --schema NAME` reads one");
            }
        }
    };
    let have = existing(&project.dir)?;

    // Which tables: named, a schema, everything, or the picker. A named
    // table the catalog lacks is read live when asked, else named.
    let names: Vec<String> = opts.tables.iter().map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()).collect();
    if !names.is_empty() {
        let missing: Vec<&String> = names
            .iter()
            .filter(|w| !cat.tables.iter().any(|t| t.physical_name().eq_ignore_ascii_case(w) || t.name.eq_ignore_ascii_case(w)))
            .collect();
        if !missing.is_empty() {
            if !opts.live {
                let list: Vec<String> = missing.iter().map(|m| m.to_string()).collect();
                bail!(
                    "the catalog has no table named {}; `zsql catalog search <pattern>` finds names, --live reads one from the warehouse",
                    list.join(", ")
                );
            }
            let read: Vec<String> = missing.iter().map(|m| m.to_string()).collect();
            crate::catalog::read(Some(&project.dir), Some(&uid), &[], &read, false)?;
            cat = catalog::load(&project.dir, &uid)?.ok_or_else(|| anyhow::anyhow!("{uid}: no catalog"))?;
        }
    }
    let today = cat.read_at.get(..10).unwrap_or(&cat.read_at).to_string();
    if let Some(s) = opts.schema {
        if !cat.tables.iter().any(|t| t.schema.as_deref().is_some_and(|ts| ts.eq_ignore_ascii_case(s))) {
            let mut have_schemas: Vec<String> = cat.tables.iter().filter_map(|t| t.schema.clone()).collect();
            have_schemas.sort();
            have_schemas.dedup();
            bail!(
                "the catalog has no schema {s} (it has: {})",
                if have_schemas.is_empty() { "none".to_string() } else { have_schemas.join(", ") }
            );
        }
    }
    let candidates: Vec<&Table> = cat.tables.iter().filter(|t| t.kind == "table" || t.kind == "view").collect();
    let tables: Vec<&Table> = if !names.is_empty() {
        candidates
            .iter()
            .copied()
            .filter(|t| names.iter().any(|w| t.physical_name().eq_ignore_ascii_case(w) || t.name.eq_ignore_ascii_case(w)))
            .collect()
    } else if let Some(s) = opts.schema {
        candidates.iter().copied().filter(|t| t.schema.as_deref().is_some_and(|ts| ts.eq_ignore_ascii_case(s))).collect()
    } else if opts.all {
        candidates.clone()
    } else {
        // The picker: every table, its state against the models that exist.
        let flag_form = "say which tables: `zsql generate orders customers`, `--schema NAME`, or `--all`; `zsql catalog search PATTERN --json` lists what the catalog holds";
        if candidates.is_empty() {
            bail!("{uid}: the catalog holds no tables; `zsql catalog read -d {uid} --schema NAME` reads some");
        }
        let find_existing = |t: &Table| {
            have.iter().find(|e| {
                e.table == t.name.to_lowercase()
                    && match (&e.schema, &t.schema) {
                        (Some(es), Some(ts)) => es == &ts.to_lowercase(),
                        _ => true,
                    }
                    && e.datasource.as_deref().is_none_or(|d| d == uid.to_lowercase())
            })
        };
        let options: Vec<String> = candidates
            .iter()
            .map(|t| {
                let state = match find_existing(t) {
                    Some(e) => {
                        let new = t
                            .columns
                            .iter()
                            .filter(|c| !e.columns.contains(&c.name.to_lowercase()) && map_type(&c.data_type) != Mapped::Skip)
                            .count();
                        if new == 0 {
                            "model exists, up to date".to_string()
                        } else {
                            format!("model exists: {new} new column{}", if new == 1 { "" } else { "s" })
                        }
                    }
                    None => String::new(),
                };
                format!(
                    "{:<40} {:<5} {:>3} columns{:<14} {state}",
                    t.physical_name(),
                    t.kind,
                    t.columns.len(),
                    t.rows.map(|r| format!(" ~{r} rows")).unwrap_or_default()
                )
            })
            .collect();
        let chosen = tty::pick_many("tables to generate", options, vec![], flag_form)?;
        if chosen.is_empty() {
            return Err(tty::Cancelled("nothing selected; nothing written").into());
        }
        chosen.into_iter().map(|i| candidates[i]).collect()
    };
    if tables.is_empty() {
        bail!("the catalog has no table matching that; `zsql catalog search <pattern>` finds names");
    }
    // What the warehouse reads as a name, and one type per field name.
    let lists = Lists::of(&cat.adapter);
    let declared = declared_types(&tables, &have);
    // A model matches a catalog table by its last two name parts, and
    // by datasource when the model says one: the same `public.orders` on
    // two warehouses is two models.
    let find = |t: &Table| {
        have.iter().find(|e| {
            e.table == t.name.to_lowercase()
                && match (&e.schema, &t.schema) {
                    (Some(es), Some(ts)) => es == &ts.to_lowercase(),
                    _ => true,
                }
                && e.datasource.as_deref().is_none_or(|d| d == uid.to_lowercase())
        })
    };

    let mut by_domain: BTreeMap<String, Vec<&Table>> = BTreeMap::new();
    for t in &tables {
        let domain = match opts.dir {
            Some(d) => d.trim().trim_matches('/').to_string(),
            None => t.schema.clone().unwrap_or_else(|| uid.clone()),
        };
        by_domain.entry(domain).or_default().push(t);
    }

    let mut created = 0;
    let mut listed = 0;
    let mut added = 0;
    for (domain, group) in &by_domain {
        let dir = project.dir.join("models").join(domain);
        for t in group {
            let rel = |p: &Path| p.strip_prefix(&project.dir).map(|p| p.display().to_string()).unwrap_or_else(|_| p.display().to_string());
            match find(t) {
                Some(e) => {
                    let absent: Vec<&Column> = t.columns.iter().filter(|c| !e.columns.contains(&c.name.to_lowercase())).collect();
                    let (new, untyped): (Vec<&Column>, Vec<&Column>) =
                        absent.into_iter().partition(|c| map_type(&c.data_type) != Mapped::Skip);
                    if !untyped.is_empty() && !opts.new_columns && opts.columns.is_empty() {
                        let names: Vec<String> = untyped.iter().map(|c| format!("{} ({})", c.name, c.data_type)).collect();
                        println!("        {}: no 0sql type, so not a field: {}", rel(&e.path), names.join(", "));
                    }
                    if new.is_empty() {
                        continue;
                    }
                    let chosen: Vec<&Column> = if opts.new_columns {
                        new.clone()
                    } else {
                        new.iter().copied().filter(|c| opts.columns.iter().any(|w| w.eq_ignore_ascii_case(&c.name))).collect()
                    };
                    if chosen.is_empty() {
                        listed += 1;
                        let names: Vec<&str> = new.iter().map(|c| c.name.as_str()).collect();
                        println!(
                            "exists  {}: {} new column{}: {}\n        zsql generate {} --columns {}   (or --new-columns)",
                            rel(&e.path),
                            new.len(),
                            if new.len() == 1 { "" } else { "s" },
                            names.join(", "),
                            t.physical_name(),
                            names.join(",")
                        );
                        continue;
                    }
                    let mut entries = String::new();
                    let mut skipped = Vec::new();
                    for c in &chosen {
                        match field(c, t, &lists, &declared) {
                            Ok(lines) => entries.push_str(&lines),
                            Err(why) => skipped.push(why),
                        }
                    }
                    for s in &skipped {
                        println!("skip    {}: {s}", rel(&e.path));
                    }
                    if entries.is_empty() {
                        continue;
                    }
                    if opts.dry_run {
                        println!("would add to {}:\n{entries}", rel(&e.path));
                    } else if append_fields(&e.path, &entries)? {
                        added += 1;
                        println!(
                            "added   {} field{} to {}",
                            chosen.len() - skipped.len(),
                            if chosen.len() - skipped.len() == 1 { "" } else { "s" },
                            rel(&e.path)
                        );
                    } else {
                        println!("{}: its fields: list is not last in the file, so paste these in yourself:\n{entries}", rel(&e.path));
                    }
                }
                None => {
                    let path = dir.join(format!("tbl.{}.yml", t.name.to_lowercase()));
                    if path.exists() {
                        println!("exists  {} (for another table; not touched)", rel(&path));
                        continue;
                    }
                    let d = table_yml(&cat, t, &today, &lists, &declared);
                    let summary = format!(
                        "{} field{}, {} measure{}{}",
                        d.fields,
                        if d.fields == 1 { "" } else { "s" },
                        d.measures,
                        if d.measures == 1 { "" } else { "s" },
                        if d.skipped.is_empty() { String::new() } else { format!(", {} skipped", d.skipped.len()) }
                    );
                    if opts.dry_run {
                        println!("would create {} ({summary})", rel(&path));
                        continue;
                    }
                    std::fs::create_dir_all(&dir)?;
                    std::fs::write(&path, d.text)?;
                    created += 1;
                    println!("created {} ({summary})", rel(&path));
                    for s in &d.skipped {
                        println!("        skipped {s}");
                    }
                }
            }
        }
        // One rel file per domain, from the foreign keys, written once.
        let rel_path = dir.join(format!("rel.{domain}.yml"));
        if let Some(text) = relation_yml(&cat, group, &today, &lists) {
            let rel = rel_path.strip_prefix(&project.dir).unwrap_or(&rel_path).display().to_string();
            if rel_path.exists() {
                // Name only the joins the file lacks, by their ON clause.
                let have = std::fs::read_to_string(&rel_path).unwrap_or_default();
                let has: serde_yaml::Value = serde_yaml::from_str(&have).unwrap_or(serde_yaml::Value::Null);
                let present: BTreeSet<String> = has
                    .as_mapping()
                    .map(|m| {
                        m.values()
                            .filter_map(|j| j.get("sql").and_then(|s| s.as_str()))
                            .map(|s| s.to_lowercase().split_whitespace().collect())
                            .collect()
                    })
                    .unwrap_or_default();
                let missing: Vec<&str> = text
                    .lines()
                    .filter(|l| l.trim_start().starts_with("sql: "))
                    .map(|l| l.trim_start().trim_start_matches("sql: ").trim_matches('"'))
                    .filter(|s| !present.contains(&s.to_lowercase().split_whitespace().collect::<String>()))
                    .collect();
                if !missing.is_empty() {
                    listed += 1;
                    println!(
                        "exists  {rel}: {} join{} the foreign keys declare and it lacks:",
                        missing.len(),
                        if missing.len() == 1 { "" } else { "s" }
                    );
                    for m in missing {
                        println!("          {m}");
                    }
                }
            } else if opts.dry_run {
                println!("would create {rel}");
            } else {
                std::fs::create_dir_all(&dir)?;
                std::fs::write(&rel_path, text)?;
                println!("created {rel}");
            }
        }
    }
    if created > 0 || added > 0 {
        eprintln!("next: zsql check, then zsql sample");
    }
    if created == 0 && listed == 0 && added == 0 && !opts.dry_run {
        println!("nothing to do: every table named has a model, with no new columns");
    }
    Ok(())
}

// ------------------------------------------------------- relationships

/// A table as its model file declares it, which is all the inference has
/// to go on: the name a join refers to it by, its datasource, and the
/// columns its fields are, with whether the file marks one a key.
struct Modeled {
    name: String,
    subject: String,
    datasource: Option<String>,
    /// (column as the file spells it, declared a primary key)
    columns: Vec<(String, bool)>,
}

/// What a table is about: its name without a warehouse's prefix or a
/// plural, which is what a key column's name is matched against.
/// `dim_households` and `Dim Household` are both about a household.
fn subject(name: &str) -> String {
    let bare = name.rsplit('.').next().unwrap_or(name);
    let mut s: String = bare.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    for p in ["dimension_", "dim_", "d_", "fact_", "fct_", "f_", "tbl_", "stg_", "raw_"] {
        if let Some(rest) = s.strip_prefix(p) {
            s = rest.to_string();
            break;
        }
    }
    if let Some(r) = s.strip_suffix("ies") {
        format!("{r}y")
    } else if ["ses", "xes", "ches", "shes"].iter().any(|e| s.ends_with(e)) {
        s[..s.len() - 2].to_string()
    } else if s.ends_with('s') && !s.ends_with("ss") {
        s[..s.len() - 1].to_string()
    } else {
        s
    }
}

/// Whether a column is shaped like a key by its name alone.
fn key_shaped(column: &str) -> bool {
    let c = column.to_lowercase();
    c == "id" || c == "key" || ["_key", "_id", "_sk", "_fk", "_code", "_num"].iter().any(|s| c.ends_with(s))
}

/// What a key column points at, when its name says: `household_key` is
/// about households, `product_id` about products.
fn key_subject(column: &str) -> Option<String> {
    let c = column.to_lowercase();
    for suffix in ["_key", "_id", "_sk", "_fk", "_code", "_num"] {
        if let Some(rest) = c.strip_suffix(suffix) {
            return (!rest.is_empty()).then(|| subject(rest));
        }
    }
    None
}

/// The subjects a column's name mentions: `coupon_upc` mentions a coupon,
/// which is how a key whose suffix nobody could enumerate is still found.
fn mentions(column: &str) -> Vec<String> {
    column.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(subject).collect()
}

/// The one column a bare-column expression names, as the file spells it.
fn bare_column(expression: &serde_yaml::Value) -> Option<(String, bool)> {
    let (sql, key) = match expression {
        serde_yaml::Value::String(s) => (s.clone(), false),
        v => (v.get("sql")?.as_str()?.to_string(), v.get("primary_key").and_then(|p| p.as_bool()).unwrap_or(false)),
    };
    let name = sql.trim();
    let inner = name.trim_matches(|c| matches!(c, '"' | '`' | '[' | ']'));
    let plain = !inner.is_empty()
        && inner.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && inner.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$' || c == ' ');
    plain.then(|| (inner.to_string(), key))
}

/// Every `tbl.*.yml` under this directory, as the inference reads them.
fn modeled(dir: &Path) -> Result<Vec<Modeled>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !(name.starts_with("tbl.") && (name.ends_with(".yml") || name.ends_with(".yaml"))) {
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(&text) else { continue };
            let Some(table) = v.get("name").and_then(|n| n.as_str()) else { continue };
            let physical = v.get("physical_name").and_then(|p| p.as_str()).unwrap_or(table);
            let columns = v
                .get("fields")
                .and_then(|f| f.as_sequence())
                .map(|fields| fields.iter().filter_map(|f| f.get("expression").and_then(bare_column)).collect())
                .unwrap_or_default();
            out.push(Modeled {
                name: table.to_string(),
                subject: subject(physical),
                datasource: v.get("datasource").and_then(|d| d.as_str()).map(|d| d.trim().to_lowercase()),
                columns,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// One inferred join: which tables, on which columns, and why.
struct Inferred {
    left: String,
    right: String,
    on: Vec<(String, String)>,
    /// `a declared key` or `the name`, for the comment.
    why: &'static str,
}

/// The joins the table files imply, and what they could not place.
/// Nothing is queried: a column that names another table in the set is
/// matched to that table's own key column, and a column that names
/// nothing here is left alone. `week_no` on a fact names no table, so it
/// never joins a date dimension it does not key.
fn infer(tables: &[Modeled]) -> (Vec<Inferred>, Vec<String>) {
    let mut joins: Vec<Inferred> = Vec::new();
    let mut loose: Vec<String> = Vec::new();
    for t in tables {
        for (column, _) in &t.columns {
            let said = mentions(column);
            let owner = tables.iter().find(|o| o.name != t.name && !o.subject.is_empty() && said.contains(&o.subject));
            let Some(o) = owner else {
                // A key-shaped column naming a table that is not here is
                // worth saying: it is a join the author still needs.
                if let Some(s) = key_subject(column).filter(|s| !tables.iter().any(|x| x.subject == *s)) {
                    loose.push(format!("{}.{column} points at a {s}, and no table here is one", t.name));
                }
                continue;
            };
            // Their side of it: the same column name, else the key they
            // declare, else a bare id.
            let theirs = o
                .columns
                .iter()
                .find(|(c, _)| c.eq_ignore_ascii_case(column))
                .or_else(|| o.columns.iter().find(|(_, key)| *key))
                .or_else(|| o.columns.iter().find(|(c, _)| c.eq_ignore_ascii_case("id") || c.eq_ignore_ascii_case("key")));
            let Some((theirs, declared)) = theirs else {
                loose.push(format!("{}.{column} points at {}, which has no key column of its own", t.name, o.name));
                continue;
            };
            // How sure: what they declare, what the name says, or that it
            // is the first column there, which a drafted dimension's key
            // nearly always is.
            let why = if *declared {
                "a declared key"
            } else if key_shaped(column) {
                "the name"
            } else if o.columns.first().is_some_and(|(first, _)| first == theirs) {
                "the name, and the first column there"
            } else {
                loose.push(format!("{}.{column} names {}, but is not a key there", t.name, o.name));
                continue;
            };
            match joins.iter_mut().find(|j| j.left == t.name && j.right == o.name) {
                // One join per pair of tables, so a second key column is
                // another part of the same condition.
                Some(j) => j.on.push((column.clone(), theirs.clone())),
                None => {
                    joins.push(Inferred { left: t.name.clone(), right: o.name.clone(), on: vec![(column.clone(), theirs.clone())], why })
                }
            }
        }
    }
    // A pair joined from both sides is one join: the model holds one per
    // pair of tables, and the many side goes left.
    let pairs: Vec<(String, String)> = joins.iter().map(|j| (j.left.clone(), j.right.clone())).collect();
    joins.retain(|j| !pairs.iter().any(|(l, r)| *l == j.right && *r == j.left && *l < j.left));
    (joins, loose)
}

pub struct RelOptions<'a> {
    /// `domain/name`, `domain`, or nothing for every domain that has tables.
    pub path: Option<&'a str>,
    pub infer: bool,
    pub datasource: Option<&'a str>,
    pub dry_run: bool,
}

/// `zsql generate --relationships`: the joins file, from the template or
/// inferred from the table files beside it.
pub fn relationships(project_dir: Option<&Path>, opts: RelOptions<'_>) -> Result<()> {
    let project = config::find_project(project_dir.unwrap_or(Path::new(".")))?;
    let models = project.dir.join("models");
    // Where the file goes, and which directory's tables it is about.
    let targets: Vec<(PathBuf, String)> = match opts.path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(path) => {
            let path = path.trim_matches('/');
            let (dir, name) = match path.rsplit_once('/') {
                Some((d, n)) => (models.join(d), n.to_string()),
                None => (models.join(path), path.to_string()),
            };
            vec![(dir, name)]
        }
        None => {
            let mut out: Vec<(PathBuf, String)> = Vec::new();
            let mut stack = vec![models.clone()];
            while let Some(d) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&d) else { continue };
                let mut has_table = false;
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("tbl.")) {
                        has_table = true;
                    }
                }
                if has_table {
                    let name = d.file_name().and_then(|n| n.to_str()).unwrap_or("core").to_string();
                    out.push((d, name));
                }
            }
            if out.is_empty() {
                bail!(
                    "no table files under {}; `zsql generate` drafts some, or name the file: --relationships <domain>/<name>",
                    models.display()
                );
            }
            out.sort();
            out
        }
    };

    for (dir, name) in &targets {
        let path = dir.join(format!("rel.{}.yml", name.to_lowercase()));
        let rel = |p: &Path| p.strip_prefix(&project.dir).unwrap_or(p).display().to_string();
        if path.exists() {
            println!("exists  {} (not touched; a joins file is yours once written)", rel(&path));
            continue;
        }
        let tables = modeled(dir)?;
        // One datasource per joins file, which is what the key declares.
        let uid = match opts.datasource {
            Some(d) => catalog::choose(&project.dir, Some(d))?.uid,
            None => {
                let mut said: Vec<String> = tables.iter().filter_map(|t| t.datasource.clone()).collect();
                said.sort();
                said.dedup();
                match said.as_slice() {
                    [one] => one.clone(),
                    [] => catalog::choose(&project.dir, None)?.uid,
                    many => bail!(
                        "the tables in {} name {} datasources ({}); a joins file is about one, so say which with -d",
                        rel(dir),
                        many.len(),
                        many.join(", ")
                    ),
                }
            }
        };
        let mut text = templates::relation_yml(&uid);
        if opts.infer {
            let mine: Vec<Modeled> = tables.into_iter().filter(|t| t.datasource.as_deref().is_none_or(|d| d == uid)).collect();
            if mine.is_empty() {
                bail!("no table files for {uid} under {}; `zsql generate` drafts some first", rel(dir));
            }
            let lists = Lists::of(&catalog::choose(&project.dir, Some(&uid))?.adapter);
            let (joins, loose) = infer(&mine);
            let mut body = String::new();
            for j in &joins {
                let on: Vec<String> = j.on.iter().map(|(l, r)| format!("left.{} = right.{}", lists.column(l), lists.column(r))).collect();
                let key = format!("{}_{}", subject(&j.left), subject(&j.right));
                body.push_str(&format!(
                    "\n{key}:\n  left: {}\n  right: {}\n  sql: {}\n{:<32}# inferred from {}: check the direction\n{:<32}# inferred: a guess, so no row is dropped by it; inner once you trust it\n",
                    yaml(&j.left),
                    yaml(&j.right),
                    yaml(&on.join(" AND ")),
                    "  cardinality: many_to_one",
                    j.why,
                    "  join: left"
                ));
            }
            let alone: Vec<&str> =
                mine.iter().filter(|t| !joins.iter().any(|j| j.left == t.name || j.right == t.name)).map(|t| t.name.as_str()).collect();
            let mut head = String::from(
                "# Inferred by `zsql generate --relationships --infer` from the table\n\
                 # files in this directory, by matching key column names. No query was\n\
                 # run and nothing here is checked against the warehouse: read every\n\
                 # line. This file is yours from here; `zsql generate` never rewrites it.\n",
            );
            if joins.is_empty() {
                head.push_str("#\n# Nothing was inferred: no column shaped like a key named another table\n# here. The examples below are the forms; write the joins by hand.\n");
            }
            if !alone.is_empty() {
                head.push_str(&format!("#\n# In no join, so they need one written by hand: {}.\n", alone.join(", ")));
            }
            for l in &loose {
                head.push_str(&format!("#   {l}\n"));
            }
            // What was inferred first, the forms under it.
            text = format!(
                "{head}datasource: {uid}\n{body}\n# Other forms, for the joins you write by hand:\n{}",
                templates::relation_examples()
            );
            if opts.dry_run {
                println!("would create {} ({} join{})\n{text}", rel(&path), joins.len(), if joins.len() == 1 { "" } else { "s" });
                continue;
            }
            std::fs::create_dir_all(dir)?;
            std::fs::write(&path, &text)?;
            println!("created {} ({} join{} inferred)", rel(&path), joins.len(), if joins.len() == 1 { "" } else { "s" });
            for j in &joins {
                println!("        {} -> {} on {}", j.left, j.right, j.on.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>().join(", "));
            }
            if !alone.is_empty() {
                println!("        in no join: {}", alone.join(", "));
            }
        } else {
            if opts.dry_run {
                println!("would create {} (the template)", rel(&path));
                continue;
            }
            std::fs::create_dir_all(dir)?;
            std::fs::write(&path, &text)?;
            println!("created {} (the template; --infer to fill it in)", rel(&path));
        }
    }
    eprintln!("next: zsql check");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_map_text_shapes_to_string_and_skip_the_rest() {
        assert_eq!(map_type("character varying(80)"), Mapped::Known("string"));
        assert_eq!(map_type("Nullable(LowCardinality(String))"), Mapped::Known("string"));
        assert_eq!(map_type("numeric(12,2)"), Mapped::Known("decimal"));
        assert_eq!(map_type("timestamp without time zone"), Mapped::Known("date_time"));
        assert_eq!(map_type("UInt64"), Mapped::Known("bigint"));
        assert_eq!(map_type("uuid"), Mapped::Text);
        assert_eq!(map_type("jsonb"), Mapped::Text);
        assert_eq!(map_type("Enum8('a' = 1)"), Mapped::Text);
        assert_eq!(map_type("text[]"), Mapped::Skip);
        assert_eq!(map_type("Array(String)"), Mapped::Skip);
        assert_eq!(map_type("interval"), Mapped::Skip);
        assert_eq!(map_type("time"), Mapped::Skip);
        assert_eq!(map_type("geometry"), Mapped::Skip);
    }

    #[test]
    fn names_and_identifiers() {
        assert_eq!(title("customer_id"), "Customer Id");
        assert_eq!(title("orderDate"), "Order Date");
        assert_eq!(title("TOTAL"), "Total");
        assert_eq!(identifiers("sum(total)"), ["total"]);
        assert_eq!(identifiers("coalesce(a, b) AND c = 1"), ["a", "b", "c"]);
        assert_eq!(yaml("Order Total"), "Order Total");
        assert_eq!(yaml("yes"), "\"yes\"");
        assert_eq!(yaml("a: b"), "\"a: b\"");
        assert_eq!(yaml("12"), "\"12\"");
    }
}

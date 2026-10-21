//! `zsql generate`: model files from a catalogue. A draft for a person,
//! never a replacement for one: an existing file is never rewritten, every
//! guess is marked on its line, and what could not be mapped is listed
//! rather than silently left out. See `docs/introspection.md`, phase C.

use crate::{config, introspect};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use zsql_source::{Catalogue, Column, Connection, Table};

pub struct Options<'a> {
    pub datasource: Option<&'a str>,
    pub schema: Option<&'a str>,
    pub table: Option<&'a str>,
    pub columns: &'a [String],
    pub new: bool,
    pub dry_run: bool,
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
fn normalise(data_type: &str) -> (String, bool) {
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
    let (t, array) = normalise(data_type);
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

/// One field's lines, or why it has none.
fn field(column: &Column, table: &Table) -> std::result::Result<String, String> {
    let (data_type, note) = match map_type(&column.data_type) {
        Mapped::Known(t) => (t, column.data_type.clone()),
        Mapped::Text => ("string", format!("{}; inferred: text-shaped, so a string", column.data_type)),
        Mapped::Skip => return Err(format!("{}: {} has no 0sql type", column.name, column.data_type)),
    };
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
            out.push_str(&format!("{:<32}# inferred: a short string, {n} wide\n", "    enable_sampling: true"));
        }
    }
    out.push_str("    expression:\n");
    if measure {
        out.push_str(&format!("      sql: sum({})\n", column.name));
    } else {
        out.push_str(&format!("      sql: {}\n", column.name));
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

fn table_yml(cat: &Catalogue, table: &Table, today: &str) -> Drafted {
    let (cost, why) = cost(table.rows);
    let mut body = String::new();
    let (mut fields, mut measures, mut skipped) = (0, 0, Vec::new());
    for column in &table.columns {
        match field(column, table) {
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
        "# Generated from catalogue/{}.json on {}. Every line ending in \"inferred\"\n\
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
fn relation_yml(cat: &Catalogue, tables: &[&Table], today: &str) -> Option<String> {
    let mut joins = String::new();
    for t in tables {
        for fk in &t.foreign_keys {
            let Some(right) = cat.table(fk.references_schema.as_deref().or(t.schema.as_deref()), &fk.references_table) else {
                continue;
            };
            let on: Vec<String> = fk.columns.iter().zip(&fk.references_columns).map(|(l, r)| format!("left.{l} = right.{r}")).collect();
            let key = format!("{}_{}", t.name, right.name).to_lowercase();
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
        "# Generated from catalogue/{}.json on {}: one join per foreign key the\n\
         # warehouse declares. Yours from here; `zsql generate` never rewrites it.\n\
         datasource: {}\n\n{joins}",
        cat.datasource, today, cat.datasource
    ))
}

/// Every model table file in the project: its path, its physical name, and
/// the column names its field expressions use.
struct Existing {
    path: PathBuf,
    physical: String,
    columns: BTreeSet<String>,
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
            let mut columns = BTreeSet::new();
            if let Some(fields) = v.get("fields").and_then(|f| f.as_sequence()) {
                for f in fields {
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
            out.push(Existing { path, physical: physical.to_lowercase(), columns });
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
    let last_key = text.lines().rev().find(|l| !l.trim().is_empty() && !l.starts_with([' ', '\t', '#', '-']));
    if last_key.map(|l| l.trim_end()) != Some("fields:") && !text.lines().any(|l| l == "fields:") {
        return Ok(false);
    }
    // The last top-level key must be fields:, so entries land in its list.
    let top: Vec<&str> = text.lines().filter(|l| !l.starts_with([' ', '\t', '#']) && l.contains(':') && !l.starts_with('-')).collect();
    if top.last().map(|l| l.trim_end()) != Some("fields:") {
        return Ok(false);
    }
    let mut out = text.clone();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(entries);
    std::fs::write(path, out)?;
    Ok(true)
}

pub fn run(project_dir: Option<&Path>, opts: Options<'_>) -> Result<()> {
    let project = config::find_project(project_dir.unwrap_or(Path::new(".")))?;
    let all = Connection::read_all(&project.dir).context("reading datasources.yml")?;
    let uid = match opts.datasource {
        Some(u) => u.trim().to_lowercase(),
        None if all.len() == 1 => all[0].uid.clone(),
        None => {
            let have: Vec<&str> = all.iter().map(|c| c.uid.as_str()).collect();
            bail!("datasources.yml names several datasources ({}); say which", have.join(", "));
        }
    };
    let cat_path = introspect::path(&project.dir, &uid);
    let text = std::fs::read_to_string(&cat_path).with_context(|| {
        format!("{}: run `zsql introspect {uid}` first", cat_path.strip_prefix(&project.dir).unwrap_or(&cat_path).display())
    })?;
    let cat: Catalogue = serde_json::from_str(&text).with_context(|| cat_path.display().to_string())?;
    let today = cat.read_at.get(..10).unwrap_or(&cat.read_at).to_string();

    let want_table = opts.table.map(|t| t.trim().to_lowercase());
    let tables: Vec<&Table> = cat
        .tables
        .iter()
        .filter(|t| t.kind == "table" || t.kind == "view")
        .filter(|t| opts.schema.is_none_or(|s| t.schema.as_deref().is_some_and(|ts| ts.eq_ignore_ascii_case(s))))
        .filter(|t| want_table.as_deref().is_none_or(|w| t.physical_name().eq_ignore_ascii_case(w) || t.name.eq_ignore_ascii_case(w)))
        .collect();
    if tables.is_empty() {
        bail!("the catalogue has no table matching that; `zsql introspect {uid}` reads it again");
    }
    let have = existing(&project.dir)?;
    let find = |t: &Table| {
        let full = t.physical_name().to_lowercase();
        have.iter().find(|e| e.physical == full || (!e.physical.contains('.') && e.physical == t.name.to_lowercase()))
    };

    let mut by_domain: BTreeMap<String, Vec<&Table>> = BTreeMap::new();
    for t in &tables {
        by_domain.entry(t.schema.clone().unwrap_or_else(|| uid.clone())).or_default().push(t);
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
                    if !untyped.is_empty() && !opts.new && opts.columns.is_empty() {
                        let names: Vec<String> = untyped.iter().map(|c| format!("{} ({})", c.name, c.data_type)).collect();
                        println!("        {}: no 0sql type, so not a field: {}", rel(&e.path), names.join(", "));
                    }
                    if new.is_empty() {
                        continue;
                    }
                    let chosen: Vec<&Column> = if opts.new {
                        new.clone()
                    } else {
                        new.iter().copied().filter(|c| opts.columns.iter().any(|w| w.eq_ignore_ascii_case(&c.name))).collect()
                    };
                    if chosen.is_empty() {
                        listed += 1;
                        let names: Vec<&str> = new.iter().map(|c| c.name.as_str()).collect();
                        println!(
                            "exists  {}: {} new column{}: {}\n        zsql generate {uid} --table {} --columns {}   (or --new)",
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
                        match field(c, t) {
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
                    let d = table_yml(&cat, t, &today);
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
        if let Some(text) = relation_yml(&cat, group, &today) {
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
    if created == 0 && listed == 0 && added == 0 && !opts.dry_run {
        println!("nothing to do: every table in the catalogue has a model, with no new columns");
    }
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

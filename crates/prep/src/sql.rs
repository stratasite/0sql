//! The `sql` step: a query the user or a client wrote, run as a black
//! box over the step before it.
//!
//! - **It sees one table, `input`:** the rows of the step before, copied
//!   into a session of its own. The source, the other steps' views and
//!   the datasets other steps read are not there, so it cannot skip the
//!   recipe or lean on names that are this crate's own.
//! - **It is one query, checked by DuckDB's own parser** before it runs:
//!   one `SELECT`, reading `input` and the tables its own `WITH` makes,
//!   and no other table, catalog or table function but `range`,
//!   `generate_series` and `unnest`. The parse is what is checked, not
//!   the text, so no quoting hides a call.
//! - **It is the same query every run.** A function whose answer is not
//!   the data's (the clock, randomness) is refused; `today()` of a
//!   formula is the one clock a recipe has. So is a function that runs
//!   other SQL or reads the engine's own state (`query`, `getenv`,
//!   `current_setting`, `duckdb_*`).
//! - **Its columns are DuckDB's answer.** It is run with no rows to
//!   learn them, and each is cast to a type a dataset has.
//! - **It is bounded** by the session's memory and threads, and stopped
//!   past [`TIME_LIMIT`].
//!
//! The session it runs in has external access off and its
//! configuration locked, as every session does: no file, no URL.

use crate::engine::{Engine, Session};
use crate::error::{Error, Result};
use crate::schema::{ident, ColumnType};
use arrow_schema::DataType;
use std::time::Duration;

/// How long a `sql` step may run.
pub const TIME_LIMIT: Duration = Duration::from_secs(60);

/// The longest query accepted.
const QUERY_LIMIT: usize = 64 * 1024;

/// Functions whose answer is not the data's.
const UNSTABLE: [&str; 16] = [
    "now",
    "today",
    "current_date",
    "current_time",
    "current_timestamp",
    "localtime",
    "localtimestamp",
    "get_current_time",
    "get_current_timestamp",
    "transaction_timestamp",
    "random",
    "setseed",
    "uuid",
    "gen_random_uuid",
    "uuidv4",
    "uuidv7",
];

/// Functions that run other SQL, or read the engine and its host.
const CLOSED: [&str; 7] = ["query", "query_table", "getenv", "current_setting", "which_secret", "glob", "sql_auto_complete"];

/// Prefixes of the same: the engine's catalogs and the readers of files.
const CLOSED_PREFIXES: [&str; 4] = ["duckdb_", "pragma_", "pg_", "read_"];

/// The table functions a query may call: rows made of nothing but
/// their arguments.
const TABLE_FUNCTIONS: [&str; 3] = ["range", "generate_series", "unnest"];

/// A `sql` step, compiled: where it reads from, and what it makes.
#[derive(Clone, Debug)]
pub struct SqlStage {
    pub step: usize,
    /// The relation in the run's session it reads, as `input`.
    pub input: String,
    pub input_columns: Vec<(String, ColumnType)>,
    pub query: String,
    pub columns: Vec<(String, ColumnType)>,
}

/// The query as it is run: with any trailing `;` gone, and not too long.
/// What it says is checked by [`vet`], which needs the engine.
pub fn checked(query: &str) -> Result<String> {
    let mut q = query.trim();
    while let Some(rest) = q.strip_suffix(';') {
        q = rest.trim_end();
    }
    if q.is_empty() {
        return Err(Error::Invalid("the query is empty".into()));
    }
    if q.len() > QUERY_LIMIT {
        return Err(Error::Invalid(format!("a query is at most {} KB", QUERY_LIMIT / 1024)));
    }
    Ok(q.to_string())
}

/// The query as DuckDB parses it, checked: one `SELECT`, reading only
/// `input` and its own `WITH` tables, calling nothing on the closed lists.
fn vet(session: &Session, query: &str) -> Result<std::result::Result<(), String>> {
    let parsed = session.text_of("SELECT json_serialize_sql(?)", query)?;
    let tree: serde_json::Value = serde_json::from_str(&parsed).map_err(|e| Error::Internal(format!("the engine's parse: {e}")))?;
    if tree["error"].as_bool() == Some(true) {
        let m = tree["error_message"].as_str().unwrap_or("it does not parse");
        return Ok(Err(if m.contains("Only SELECT") { "a sql step is one query, a SELECT".into() } else { plain(m) }));
    }
    if tree["statements"].as_array().map_or(0, Vec::len) != 1 {
        return Ok(Err("a sql step is one query, a SELECT".into()));
    }
    Ok(walk(&tree, &[]))
}

/// The names a node's own `WITH` makes. They are seen in that node and
/// below it, its subqueries and the `WITH` tables' own bodies, and
/// nowhere else: a name made inside a subquery is not a table outside it.
fn ctes_of(o: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    o.get("cte_map")
        .and_then(|m| m["map"].as_array())
        .map(|map| map.iter().filter_map(|e| e["key"].as_str()).map(str::to_lowercase).collect())
        .unwrap_or_default()
}

fn walk(v: &serde_json::Value, outer: &[String]) -> std::result::Result<(), String> {
    match v {
        serde_json::Value::Object(o) => {
            let own = ctes_of(o);
            let scope: Vec<String>;
            let ctes = if own.is_empty() {
                outer
            } else {
                scope = outer.iter().cloned().chain(own).collect();
                &scope
            };
            let text = |k: &str| o.get(k).and_then(|x| x.as_str()).unwrap_or("").to_lowercase();
            match (text("type").as_str(), text("class").as_str()) {
                ("base_table", _) => {
                    let (table, schema, catalog) = (text("table_name"), text("schema_name"), text("catalog_name"));
                    if !schema.is_empty() || !catalog.is_empty() || !(table == "input" || ctes.contains(&table)) {
                        let shown = [catalog, schema, table].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(".");
                        return Err(format!("it reads {shown}; a sql step reads input, and the tables its own WITH makes"));
                    }
                }
                ("table_function", _) => {
                    let f = o.get("function").and_then(|f| f["function_name"].as_str()).unwrap_or("").to_lowercase();
                    if !TABLE_FUNCTIONS.contains(&f.as_str()) {
                        return Err(format!(
                            "{f}() is not a table a sql step reads; it reads input, and range(), generate_series() and unnest()"
                        ));
                    }
                }
                ("show_ref", _) => return Err("a sql step is one query, a SELECT".into()),
                // `current_date` and its kind are written without parentheses.
                (_, "column_ref") => {
                    if let Some([only]) = o.get("column_names").and_then(|c| c.as_array()).map(Vec::as_slice) {
                        let name = only.as_str().unwrap_or("").to_lowercase();
                        if UNSTABLE.contains(&name.as_str()) {
                            closed(&name)?;
                        }
                    }
                }
                _ => {}
            }
            // Every node that calls something by name is checked, of
            // whatever class: a function, an aggregate, a window.
            if let Some(f) = o.get("function_name").and_then(|f| f.as_str()) {
                if !text("schema").is_empty() || !text("catalog").is_empty() {
                    return Err(format!("{}() is named with a schema; a sql step calls DuckDB's functions by their names", f.to_lowercase()));
                }
                closed(&f.to_lowercase())?;
            }
            o.values().try_for_each(|x| walk(x, ctes))
        }
        serde_json::Value::Array(a) => a.iter().try_for_each(|x| walk(x, outer)),
        _ => Ok(()),
    }
}

/// Refused when a function is on a closed list.
fn closed(f: &str) -> std::result::Result<(), String> {
    if UNSTABLE.contains(&f) {
        return Err(format!(
            "{f} is not the same on every run, and a recipe runs again on every new file; use a column of the data, or a formula step's today()"
        ));
    }
    if CLOSED.contains(&f) || CLOSED_PREFIXES.iter().any(|p| f.starts_with(p)) {
        return Err(format!("{f}() reads the engine or runs other SQL, which a sql step does not; it reads input"));
    }
    Ok(())
}

/// An engine refusal in the user's terms: without the excerpt of the
/// query as it was wrapped, whose positions are not the user's.
fn plain(message: &str) -> String {
    let cut = message.find("\nLINE ").unwrap_or(message.len());
    message[..cut].trim().to_string()
}

/// The query as a subquery: one query, whatever was written. The newline
/// keeps a trailing `--` comment from swallowing the parenthesis.
fn wrapped(query: &str) -> String {
    format!("({query}\n) AS \"__q\"")
}

/// What a query makes over `input` with these columns, learned by running
/// it with no rows. `Ok(Err(..))` is the engine's refusal, in its words.
pub fn describe(
    engine: &Engine,
    input: &[(String, ColumnType)],
    query: &str,
) -> Result<std::result::Result<Vec<(String, ColumnType)>, String>> {
    let session = engine.session()?;
    if let Err(refusal) = vet(&session, query)? {
        return Ok(Err(refusal));
    }
    session.load("input", input, &[])?;
    let rows = match session.query_within(&format!("SELECT * FROM {} LIMIT 0", wrapped(query)), Duration::from_secs(10)) {
        Ok(rows) => rows,
        Err(e) => return Ok(Err(plain(&e))),
    };
    // The engine's own names for the types, which tell a sum's HUGEINT
    // from a DECIMAL(38,0) that Arrow holds alike.
    let named = session
        .texts(&format!("SELECT column_type FROM (DESCRIBE SELECT * FROM {})", wrapped(query)))
        .unwrap_or_default();
    let mut out: Vec<(String, ColumnType)> = Vec::new();
    for (i, f) in rows.schema.fields().iter().enumerate() {
        let name = f.name().clone();
        if out.iter().any(|(n, _)| n.eq_ignore_ascii_case(&name)) {
            return Ok(Err(format!("it makes two columns named '{name}'; give each its own name with AS")));
        }
        match type_of(f.data_type(), named.get(i).map(String::as_str).unwrap_or("")) {
            Some(t) => out.push((name, t)),
            None => {
                return Ok(Err(format!(
                    "column '{name}' is {}, which a dataset does not hold; CAST it to text, a number, a date or a timestamp",
                    f.data_type()
                )))
            }
        }
    }
    if out.is_empty() {
        return Ok(Err("it makes no column".into()));
    }
    Ok(Ok(out))
}

/// A column the engine answers with, as a dataset's type, given the
/// engine's name for it.
fn type_of(t: &DataType, named: &str) -> Option<ColumnType> {
    match t {
        // A sum of integers is a HUGEINT, which comes back as a decimal
        // of no places; it is a whole number. A DECIMAL(38,0) of the
        // data's own, 20-digit numbers, stays one, as does a UBIGINT.
        DataType::Decimal128(_, 0) | DataType::Decimal256(_, 0) if named.eq_ignore_ascii_case("hugeint") => Some(ColumnType::Integer),
        DataType::Decimal128(_, 0) | DataType::Decimal256(_, 0) | DataType::UInt64 => Some(ColumnType::Decimal(0)),
        DataType::Timestamp(_, _) => Some(ColumnType::Timestamp),
        DataType::Null => Some(ColumnType::Text),
        other => ColumnType::from_arrow(other).ok(),
    }
}

/// The column a refusal says is not there, when that is what it says.
pub fn missing_column(message: &str) -> Option<String> {
    let at = message.find("Referenced column \"")? + "Referenced column \"".len();
    let rest = &message[at..];
    Some(rest[..rest.find('"')?].to_string())
}

/// Runs a `sql` step: its input copied into a session of its own as
/// `input`, the query run there, and what it makes loaded back as `name`.
pub fn run(engine: &Engine, session: &Session, name: &str, stage: &SqlStage) -> Result<()> {
    let rows = session.query(&format!("SELECT * FROM {}", stage.input))?;
    // No more time than the job has left.
    let own = engine.session_until(session.deadline())?;
    own.load("input", &stage.input_columns, &rows.batches)?;
    let list: Vec<String> = stage.columns.iter().map(|(c, t)| format!("CAST(\"__q\".{x} AS {}) AS {x}", t.sql(), x = ident(c))).collect();
    let made = own
        .query_within(&format!("SELECT {} FROM {}", list.join(", "), wrapped(&stage.query)), TIME_LIMIT)
        .map_err(|e| Error::Unprocessable(format!("step {} (sql): {}", stage.step, plain(&e))))?;
    session.load(name, &stage.columns, &made.batches)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vetted(q: &str) -> std::result::Result<(), String> {
        let s = crate::engine::tests::engine().session().unwrap();
        vet(&s, &checked(q).map_err(|e| e.to_string())?).unwrap()
    }

    #[test]
    fn a_query_is_one_select_over_input_checked_as_parsed() {
        assert_eq!(checked("SELECT * FROM input;  ;").unwrap(), "SELECT * FROM input");
        assert!(checked("  ;").is_err());
        for q in [
            "SELECT * FROM input",
            "WITH a AS (SELECT * FROM input), b AS (SELECT * FROM a) SELECT * FROM b",
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 3) SELECT * FROM input, n",
            "SELECT *, i FROM input, range(3) r(i)",
            "SELECT 'now() random() pg_catalog' AS \"today\" FROM input -- now()\n",
            "SELECT unnest([1, 2]) AS x",
            "SELECT * FROM (WITH a AS (SELECT * FROM input) SELECT * FROM a, (SELECT * FROM a) b) t",
            "SELECT *, row_number() OVER () AS n FROM input",
        ] {
            assert_eq!(vetted(q), Ok(()), "{q}");
        }
        for (q, says) in [
            ("SELECT 1; SELECT 2", "one query"),
            ("DROP TABLE input", "one query, a SELECT"),
            ("SELECT now() AS t FROM input", "now is not the same"),
            ("SELECT \"now\"() AS t FROM input", "now is not the same"),
            ("SELECT current_date AS d FROM input", "current_date is not the same"),
            ("SELECT RANDOM() AS r FROM input", "random is not the same"),
            // Quoting that a scan of the text would misread.
            ("SELECT $$'$$ AS x, now() AS t, '$$' AS y FROM input", "now is not the same"),
            ("SELECT E'\\'' AS x, now() AS t, '' AS y FROM input", "now is not the same"),
            ("SELECT * FROM query('SELECT now()')", "query() is not a table"),
            ("SELECT * FROM s1", "it reads s1"),
            ("SELECT * FROM pg_catalog.pg_class", "it reads pg_catalog.pg_class"),
            ("SELECT * FROM information_schema.tables", "it reads information_schema.tables"),
            ("SELECT * FROM main.input", "it reads main.input"),
            ("SELECT * FROM duckdb_settings()", "duckdb_settings() is not a table"),
            ("SELECT current_setting('temp_directory') AS t", "current_setting() reads the engine"),
            ("SELECT getenv('HOME') AS h", "getenv() reads the engine"),
            ("SELECT * FROM read_csv('/etc/passwd')", "read_csv() is not a table"),
            ("SELECT (SELECT count(*) FROM duckdb_tables()) AS n FROM input", "duckdb_tables() is not a table"),
            ("SELECT pg_catalog.now() AS t", "named with a schema"),
            // A WITH name is a table only where its WITH is seen.
            ("SELECT name, setting FROM (WITH pg_settings AS (SELECT 1) SELECT 1) t, pg_settings", "it reads pg_settings"),
            ("SELECT * FROM (WITH duckdb_tables AS (SELECT 1) SELECT 1) t, duckdb_tables", "it reads duckdb_tables"),
            ("SELECT current_setting('temp_directory') OVER () AS t FROM input", "current_setting() reads the engine"),
            ("SHOW TABLES", "one query"),
        ] {
            let got = vetted(q);
            assert!(got.as_ref().err().is_some_and(|e| e.contains(says)), "{q}: {got:?}");
        }
    }

    #[test]
    fn a_refusal_names_the_column_it_could_not_find_and_not_our_wrapping() {
        let m = "Binder Error: Referenced column \"store\" not found in FROM clause!";
        assert_eq!(missing_column(m).as_deref(), Some("store"));
        assert_eq!(missing_column("Parser Error: syntax error"), None);
        let wrapped = "Catalog Error: Table with name s1 does not exist!\n\nLINE 1: SELECT * FROM (SELECT * FROM s1\n                               ^";
        assert_eq!(plain(wrapped), "Catalog Error: Table with name s1 does not exist!");
    }
}

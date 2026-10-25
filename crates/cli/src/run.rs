//! `zsql run`: a planned query, run on the datasource the planner chose,
//! its rows kept and printed. And `zsql results`: what was kept. Both
//! local only, like `sample`: the credentials are on this machine. See
//! `docs/run.md`.

use crate::config::{self, Project};
use crate::local;
use crate::side::Query;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use zsql_exec::{CacheStatus, Connector, Engine, Limit, Manifest, RunRequest, Store};
use zsql_service::{Model, SqlRequest};
use zsql_source::{Connection, Source};

/// Where a project's results live: `ZSQL_RESULTS_DIR`, else
/// `<home>/results/<project uid>`; `None` when `ZSQL_RESULTS=off`.
pub fn results_root(project: &Project) -> Option<PathBuf> {
    if std::env::var("ZSQL_RESULTS").is_ok_and(|v| v.eq_ignore_ascii_case("off")) {
        return None;
    }
    if let Some(dir) = std::env::var_os("ZSQL_RESULTS_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir).join(&project.uid));
    }
    config::home().map(|h| h.join("results").join(&project.uid))
}

/// The command line's connector: `datasources.yml` and `.zsql`.
struct Local_ {
    dir: PathBuf,
    timeout: Option<u64>,
}

impl Connector for Local_ {
    fn open(&self, uid: &str) -> zsql_source::Result<Box<dyn Source>> {
        let mut conn = Connection::read(&self.dir, uid)?;
        // --timeout for this run, else the datasource's, else the project's, else ten minutes.
        conn.query_timeout = self.timeout.map(|t| t as i64).or(conn.query_timeout).or_else(|| project_timeout(&self.dir)).or(Some(600));
        let secrets = zsql_source::secrets(Some(&self.dir), &conn.uid);
        zsql_source::open(&conn, secrets)
    }

    /// What the cache key uses to tell one connection from another, and
    /// so what keeps a result from being served for a different database.
    ///
    /// Everything that decides which data answers goes in, not the first
    /// of a few fields: a file datasource has no username, account or
    /// project, so it used to contribute nothing at all and two DuckDB
    /// files behind one datasource key hashed the same. A Postgres one
    /// contributed only its username, so the same user against staging
    /// and production hashed the same too.
    ///
    /// A file goes in by its canonical path and by its size and mtime,
    /// because `./data.duckdb` names a different database from each
    /// directory it is read in, and because the file a job rewrites
    /// overnight is a different database from the one cached yesterday.
    /// A path that cannot be canonicalised or stat'd is used as written:
    /// no worse than naming nothing, and the read will fail anyway.
    ///
    /// Written as JSON so it cannot be misread: joined by spaces, a
    /// `database` of `x username=y` is indistinguishable from a database
    /// `x` beside a username `y`, and the two hash alike.
    ///
    /// Nothing secret belongs here. These are names, locations and
    /// timestamps, and the identity is written into the result's
    /// manifest and printed by `zsql results --json`.
    ///
    /// What is still missing is a principal the file does not name: an
    /// AWS profile from the environment, or a token in `.zsql` that
    /// decides which rows a warehouse shows. Those want a digest of the
    /// resolved credential, which is a decision about what may be
    /// written to disk, so they are not here yet.
    fn identity(&self, uid: &str) -> Option<String> {
        let c = Connection::read(&self.dir, uid).ok()?;
        // A file by where it is and what it is: two different databases
        // have been served each other's rows both ways.
        let file = |p: Option<std::path::PathBuf>| {
            let p = p?;
            let at = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
            let stamp = std::fs::metadata(&at).ok().map(|m| {
                let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
                format!("@{modified}:{}", m.len())
            });
            Some(format!("{}{}", at.display(), stamp.unwrap_or_default()))
        };
        let mut parts = serde_json::Map::new();
        let mut put = |name: &str, value: Option<String>| {
            if let Some(v) = value {
                parts.insert(name.to_string(), serde_json::Value::String(v));
            }
        };
        put("host", c.host);
        put("port", c.port.map(|p| p.to_string()));
        put("database", c.database);
        put("schema", c.schema);
        put("username", c.username);
        put("url", c.url);
        put("file", file(c.file));
        put("account", c.account);
        put("warehouse", c.warehouse);
        put("role", c.role);
        put("catalog", c.catalog);
        put("client_id", c.client_id);
        put("project", c.project);
        put("location", c.location);
        put("region", c.region);
        put("workgroup", c.workgroup);
        put("profile", c.profile);
        // The credential's own file, which picks the principal and so the
        // rows, for BigQuery. Its contents are not read here.
        put("key_file", file(c.key_file));
        (!parts.is_empty()).then(|| serde_json::Value::Object(parts).to_string())
    }

    fn cache_ttl(&self, uid: &str) -> Option<Duration> {
        Connection::read(&self.dir, uid).ok()?.cache_ttl.map(|s| Duration::from_secs(s.max(0) as u64))
    }
}

/// `query_timeout` in project.yml, when there is one.
fn project_timeout(dir: &Path) -> Option<i64> {
    let text = std::fs::read_to_string(dir.join("project.yml")).ok()?;
    let v: serde_yaml::Value = serde_yaml::from_str(&text).ok()?;
    v.get("query_timeout").and_then(|t| t.as_i64()).filter(|t| *t > 0)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Table,
    Json,
    Jsonl,
    Csv,
}

impl Format {
    pub fn parse(s: &str) -> Result<Format> {
        Ok(match s.to_lowercase().as_str() {
            "table" => Format::Table,
            "json" => Format::Json,
            "jsonl" | "ndjson" => Format::Jsonl,
            "csv" => Format::Csv,
            other => bail!("--format {other} is not one of table, json, jsonl, csv"),
        })
    }

    /// From a file's extension, for `--out`.
    fn of_path(p: &Path) -> Option<Format> {
        match p.extension()?.to_str()?.to_lowercase().as_str() {
            "json" => Some(Format::Json),
            "jsonl" | "ndjson" => Some(Format::Jsonl),
            "csv" => Some(Format::Csv),
            _ => None,
        }
    }
}

impl Default for Options<'_> {
    /// What the repl runs with: the cap, the cache, a table, fifty rows.
    fn default() -> Self {
        Options { limit: None, all: false, fresh: false, timeout: None, format: None, out: None, head: Some(50), quiet: false }
    }
}

pub struct Options<'a> {
    pub limit: Option<u64>,
    pub all: bool,
    pub fresh: bool,
    pub timeout: Option<u64>,
    pub format: Option<&'a str>,
    pub out: Option<&'a Path>,
    pub head: Option<usize>,
    pub quiet: bool,
}

pub fn run(project: &Project, model: &Model, query: &Query, context: Option<&Value>, opts: Options<'_>) -> Result<()> {
    let req = SqlRequest { spec: query.spec().cloned(), expr: query.expr().map(str::to_string), context: local::context_of(context)? };
    let cap = std::env::var("ZSQL_RUN_CAP").ok().and_then(|c| c.parse().ok()).unwrap_or(zsql_exec::DEFAULT_CAP);
    let limit = if opts.all { Limit::All } else { Limit::Rows(opts.limit.unwrap_or(cap)) };
    let root = results_root(project);
    let engine = Engine::open(root.as_deref())?;
    let connector = Local_ { dir: project.dir.clone(), timeout: opts.timeout };
    let outcome = engine.run(model, &connector, RunRequest { query: req, limit, fresh: opts.fresh, store: root.is_some() })?;

    let format = match (opts.format, opts.out) {
        (Some(f), _) => Format::parse(f)?,
        (None, Some(p)) => {
            Format::of_path(p).ok_or_else(|| anyhow::anyhow!("{}: say --format for this extension (json, jsonl, csv)", p.display()))?
        }
        (None, None) => Format::Table,
    };
    let tty = unsafe { libc::isatty(1) } == 1;
    let head = opts.head.or(if format == Format::Table && tty && opts.out.is_none() { Some(50) } else { None });
    match opts.out {
        Some(path) => {
            let mut file = std::fs::File::create(path).with_context(|| path.display().to_string())?;
            emit(&mut file, format, &outcome.manifest, &outcome.rows, head, &outcome.cache)?;
            if !opts.quiet {
                eprintln!("{} -> {}", footer(&outcome.manifest, &outcome.cache, head), path.display());
            }
        }
        None => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            emit(&mut lock, format, &outcome.manifest, &outcome.rows, head, &outcome.cache)?;
            if !opts.quiet && format == Format::Table {
                eprintln!("{}", footer(&outcome.manifest, &outcome.cache, head));
            }
        }
    }
    if !opts.quiet {
        for w in &outcome.manifest.warnings {
            eprintln!("warning: {w}");
        }
    }
    Ok(())
}

/// The line under a table: rows, completeness, where it ran, how long,
/// the id, and whether it came from the cache.
fn footer(m: &Manifest, cache: &CacheStatus, head: Option<usize>) -> String {
    let rows = match (m.complete, m.user_limit) {
        (true, Some(n)) => format!("{} rows (your limit {n})", m.rows),
        (true, None) => format!("{} rows (complete)", m.rows),
        (false, _) => format!("{} of more (--limit {} or --all)", m.rows, m.cap.unwrap_or(0) * 5),
    };
    let shown = match head {
        Some(h) if (h as u64) < m.rows => format!(", {h} shown (--head N for more)"),
        _ => String::new(),
    };
    let mut line = format!("{rows}{shown} · {} ({}) · {:.1} s · {}", m.datasource_uid, m.adapter, m.timings.run_ms as f64 / 1e3, m.id);
    match cache {
        CacheStatus::Hit { age } => {
            line.push_str(&format!(" · cached {}, saved {:.1} s; --fresh to run again", ago(*age), m.timings.run_ms as f64 / 1e3))
        }
        CacheStatus::Bypassed => line.push_str(" · ran fresh"),
        CacheStatus::Disabled => line.push_str(" · not kept"),
        CacheStatus::Miss => {}
    }
    line
}

fn ago(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s} s ago")
    } else if s < 3600 {
        format!("{} min ago", s / 60)
    } else if s < 86_400 {
        format!("{} h ago", s / 3600)
    } else {
        format!("{} d ago", s / 86_400)
    }
}

/// The rows in one of the four shapes. Every machine format is explicit
/// and never changes with where stdout goes.
pub fn emit(
    w: &mut dyn Write,
    format: Format,
    m: &Manifest,
    rows: &[Vec<Option<String>>],
    head: Option<usize>,
    cache: &CacheStatus,
) -> Result<()> {
    let names: Vec<&str> = m.columns.iter().map(|c| c.name.as_str()).collect();
    let shown: &[Vec<Option<String>>] = match head {
        Some(h) if h < rows.len() => &rows[..h],
        _ => rows,
    };
    match format {
        Format::Table => table(w, &names, &m.columns, shown)?,
        Format::Json => {
            let cache = match cache {
                CacheStatus::Hit { age } => json!({ "hit": true, "age_s": age.as_secs() }),
                CacheStatus::Miss => json!({ "hit": false }),
                CacheStatus::Bypassed => json!({ "hit": false, "bypassed": true }),
                CacheStatus::Disabled => json!({ "hit": false, "disabled": true }),
            };
            let v = json!({ "manifest": m, "columns": names, "rows": shown, "cache": cache });
            writeln!(w, "{}", serde_json::to_string_pretty(&v)?)?;
        }
        Format::Jsonl => {
            for r in shown {
                let obj: serde_json::Map<String, Value> =
                    names.iter().zip(r).map(|(n, c)| (n.to_string(), c.clone().map(Value::String).unwrap_or(Value::Null))).collect();
                writeln!(w, "{}", Value::Object(obj))?;
            }
        }
        Format::Csv => {
            writeln!(w, "{}", names.iter().map(|n| csv_cell(Some(n))).collect::<Vec<_>>().join(","))?;
            for r in shown {
                writeln!(w, "{}", r.iter().map(|c| csv_cell(c.as_deref())).collect::<Vec<_>>().join(","))?;
            }
        }
    }
    Ok(())
}

/// RFC 4180: empty for null, quoted when a comma, a quote or a newline
/// is inside, quotes doubled.
fn csv_cell(c: Option<&str>) -> String {
    match c {
        None => String::new(),
        Some(s) if s.contains([',', '"', '\n', '\r']) => format!("\"{}\"", s.replace('"', "\"\"")),
        Some(s) => s.to_string(),
    }
}

/// A terminal table: widths over the first two hundred rows, cells cut
/// at forty, numbers right-aligned, null shown as ∅.
fn table(w: &mut dyn Write, names: &[&str], columns: &[zsql_exec::Column], rows: &[Vec<Option<String>>]) -> Result<()> {
    const MAX: usize = 40;
    let cut = |s: &str| -> String {
        if s.chars().count() > MAX {
            let mut t: String = s.chars().take(MAX - 1).collect();
            t.push('…');
            t
        } else {
            s.to_string()
        }
    };
    let cell = |c: &Option<String>| match c {
        None => "∅".to_string(),
        Some(s) => cut(s),
    };
    let numeric: Vec<bool> = (0..names.len())
        .map(|i| {
            let typed = columns.get(i).and_then(|c| c.data_type.as_deref()).is_some_and(|t| matches!(t, "integer" | "bigint" | "decimal"));
            typed
                || rows.iter().take(200).filter_map(|r| r.get(i).and_then(|c| c.as_deref())).all(|v| v.parse::<f64>().is_ok())
                    && rows.iter().take(200).any(|r| r.get(i).is_some_and(|c| c.is_some()))
        })
        .collect();
    let mut widths: Vec<usize> = names.iter().map(|n| n.chars().count()).collect();
    for r in rows.iter().take(200) {
        for (i, c) in r.iter().enumerate() {
            if let Some(wd) = widths.get_mut(i) {
                *wd = (*wd).max(cell(c).chars().count());
            }
        }
    }
    let line = |cells: Vec<String>| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let pad = widths.get(i).copied().unwrap_or(0).saturating_sub(c.chars().count());
                if numeric.get(i).copied().unwrap_or(false) {
                    format!("{}{c}", " ".repeat(pad))
                } else {
                    format!("{c}{}", " ".repeat(pad))
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    };
    writeln!(w, "{}", line(names.iter().map(|n| n.to_string()).collect()).trim_end())?;
    writeln!(w, "{}", widths.iter().map(|wd| "─".repeat(*wd)).collect::<Vec<_>>().join("  "))?;
    for r in rows {
        writeln!(w, "{}", line(r.iter().map(cell).collect()).trim_end())?;
    }
    Ok(())
}

// ------------------------------------------------------------------ results

pub enum ResultsCommand<'a> {
    List { json: bool },
    Show { id: &'a str, format: Option<&'a str>, head: Option<usize>, offset: usize },
    Export { id: &'a str, out: &'a Path },
    Sql { id: &'a str },
    Remove { ids: &'a [String] },
    Purge { yes: bool },
}

pub fn results(project: &Project, cmd: ResultsCommand<'_>) -> Result<()> {
    let Some(root) = results_root(project) else {
        bail!("ZSQL_RESULTS=off: nothing is kept, so there is nothing to list");
    };
    let store = Store::open(&root)?;
    match cmd {
        ResultsCommand::List { json } => {
            let all = store.list()?;
            if json {
                let v: Vec<&Manifest> = all.iter().map(|s| &s.manifest).collect();
                println!("{}", serde_json::to_string_pretty(&json!({ "root": root, "results": v }))?);
                return Ok(());
            }
            if all.is_empty() {
                println!("no results under {}", root.display());
                return Ok(());
            }
            println!("{:<22} {:<20} {:>8}  {:<9} {:<12} query", "id", "when", "rows", "complete", "datasource");
            for s in &all {
                let m = &s.manifest;
                let what = m.expr.clone().or_else(|| m.spec.as_ref().map(|v| v.to_string())).unwrap_or_default();
                let what: String = what.chars().take(60).collect();
                println!(
                    "{:<22} {:<20} {:>8}  {:<9} {:<12} {}",
                    m.id,
                    m.created_at,
                    m.rows,
                    if m.complete { "yes" } else { "no" },
                    m.datasource_uid,
                    what
                );
            }
            eprintln!("{} results under {}; `zsql results show <id>`, `zsql results purge`", all.len(), root.display());
        }
        ResultsCommand::Show { id, format, head, offset } => {
            let s = store.get(id)?;
            let rows: Vec<Vec<Option<String>>> = s.rows()?.skip(offset).collect::<zsql_exec::Result<_>>()?;
            let format = format.map(Format::parse).transpose()?.unwrap_or(Format::Table);
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            emit(&mut lock, format, &s.manifest, &rows, head, &CacheStatus::Hit { age: s.age() })?;
            if format == Format::Table {
                eprintln!("{}", footer(&s.manifest, &CacheStatus::Hit { age: s.age() }, head));
            }
        }
        ResultsCommand::Export { id, out } => {
            let s = store.get(id)?;
            let rows: Vec<Vec<Option<String>>> = s.rows()?.collect::<zsql_exec::Result<_>>()?;
            let format = Format::of_path(out)
                .ok_or_else(|| anyhow::anyhow!("{}: say the format by the extension (json, jsonl, csv)", out.display()))?;
            let mut file = std::fs::File::create(out).with_context(|| out.display().to_string())?;
            emit(&mut file, format, &s.manifest, &rows, None, &CacheStatus::Hit { age: s.age() })?;
            eprintln!("{} rows -> {}", rows.len(), out.display());
        }
        ResultsCommand::Sql { id } => {
            let s = store.get(id)?;
            println!("-- {} on {} ({})", s.manifest.id, s.manifest.datasource_uid, s.manifest.adapter);
            println!("{}", s.manifest.sql);
        }
        ResultsCommand::Remove { ids } => {
            if ids.is_empty() {
                bail!("name the results to remove, by id");
            }
            for id in ids {
                let s = store.get(id)?;
                store.remove(&s.manifest.key)?;
                eprintln!("removed {}", s.manifest.id);
            }
        }
        ResultsCommand::Purge { yes } => {
            let n = store.list()?.len();
            if n > 0 && !yes && !crate::tty::confirm(&format!("remove all {n} results under {}?", root.display()), "--yes")? {
                bail!("nothing removed");
            }
            let n = store.purge()?;
            eprintln!("purged {n} result{} under {}", if n == 1 { "" } else { "s" }, root.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zsql_exec::Connector;

    fn identity_of(yaml: &str, name: &str) -> Option<String> {
        let dir = std::env::temp_dir().join(format!("zsql-identity-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("datasources.yml"), yaml).unwrap();
        let got = Local_ { dir: dir.clone(), timeout: None }.identity("wh");
        let _ = std::fs::remove_dir_all(&dir);
        got
    }

    /// The identity decides whether one database's rows can be served for
    /// another's, so it has to say exactly which database, and say it in a
    /// form that cannot be read two ways.
    #[test]
    fn an_identity_names_one_database_and_only_one() {
        // Everything that picks the data is named.
        let pg = identity_of(
            "wh:\n  adapter: postgres\n  host: db.example\n  port: 5432\n  database: analytics\n  schema: shop\n  username: reader\n",
            "pg",
        )
        .expect("an identity");
        for named in ["db.example", "5432", "analytics", "shop", "reader"] {
            assert!(pg.contains(named), "{named} is part of which data answers: {pg}");
        }

        // Staging and production under one username are not one database.
        let staging = identity_of("wh:\n  adapter: postgres\n  host: staging\n  database: analytics\n  username: reader\n", "s");
        let prod = identity_of("wh:\n  adapter: postgres\n  host: prod\n  database: analytics\n  username: reader\n", "p");
        assert_ne!(staging, prod, "the host decides which rows come back");

        // Nor are two databases on one host.
        let a = identity_of("wh:\n  adapter: postgres\n  host: db\n  database: a\n  username: reader\n", "a");
        let b = identity_of("wh:\n  adapter: postgres\n  host: db\n  database: b\n  username: reader\n", "b");
        assert_ne!(a, b, "the database decides which rows come back");

        // A value holding the separator cannot impersonate another field:
        // joined by spaces, `database: "x username=y"` read the same as a
        // database `x` beside a username `y`, and the two hashed alike.
        let sneaky = identity_of("wh:\n  adapter: postgres\n  database: \"x username=y\"\n", "sneaky");
        let plain = identity_of("wh:\n  adapter: postgres\n  database: x\n  username: y\n", "plain");
        assert_ne!(sneaky, plain, "a value cannot be read as a second field:\n  {sneaky:?}\n  {plain:?}");

        // Nothing at all to say is no identity, not an empty one.
        assert_eq!(identity_of("wh:\n  adapter: duckdb\n", "bare"), None);
    }

    /// A file datasource is the case that started this: the path alone was
    /// not in the key at all, and then the path alone could not tell a
    /// rebuilt database from the one cached yesterday.
    ///
    /// The path is written absolute here because an adapter opens
    /// `conn.file` as it stands, so a relative one resolves against the
    /// process's directory; the identity canonicalises against the same
    /// one, which is what keeps it naming the file that was opened. A
    /// test does not run from a project, so a relative path would be
    /// testing the fallback rather than the resolution.
    #[test]
    fn a_file_is_named_by_where_it_is_and_what_it_is() {
        let dir = std::env::temp_dir().join(format!("zsql-identity-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("data.duckdb");
        std::fs::write(&db, b"one").unwrap();
        std::fs::write(dir.join("datasources.yml"), format!("wh:\n  adapter: duckdb\n  file: {}\n", db.display())).unwrap();
        let local = Local_ { dir: dir.clone(), timeout: None };

        let before = local.identity("wh").expect("an identity");
        assert!(before.contains(&db.canonicalize().unwrap().display().to_string()), "the canonical path: {before}");

        // The same file again is the same database, or nothing is cached.
        assert_eq!(local.identity("wh"), Some(before.clone()), "an unchanged file still hits the cache");

        // A rewritten one is not, which is what a nightly job produces.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&db, b"two and longer").unwrap();
        assert_ne!(local.identity("wh"), Some(before), "a rewritten database is a different database");

        // A path that cannot be resolved is used as written rather than
        // dropped. Such a query cannot succeed, so nothing is cached under
        // it: both adapters refuse a file they cannot open.
        std::fs::write(dir.join("datasources.yml"), "wh:\n  adapter: duckdb\n  file: ./missing.duckdb\n").unwrap();
        let missing = local.identity("wh").expect("an identity even so");
        assert!(missing.contains("missing.duckdb"), "{missing}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

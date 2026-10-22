//! `zsql catalog`: read a datasource's catalog and write it beside the
//! model. Local only: the server never holds a warehouse credential.
//!
//! The file is a union with its provenance: the schemas read whole, each
//! with when, and the tables read one by one. A sync re-reads every one
//! of those and replaces what each read covers; it never drops a table
//! on a failed read, only on a read that succeeded and did not find it.
//! `search` looks in the file, or in the warehouse with `--live`; `add`
//! reads named tables and appends them. See `docs/introspection.md`.

use crate::config;
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::time::Instant;
use zsql_source::{Catalog, Connection, Drift, Filter, Source};

/// Where a datasource's catalog is written: one file per datasource,
/// so re-reading one never rewrites another, and committed rather than
/// gitignored, because it is schema and a diff in review is where drift
/// wants to be seen.
pub fn path(project_dir: &Path, uid: &str) -> std::path::PathBuf {
    project_dir.join("catalog").join(format!("{uid}.json"))
}

/// The datasource a command means: the one named, or the only one.
fn choose(project_dir: &Path, datasource: Option<&str>) -> Result<Connection> {
    let all = Connection::read_all(project_dir).context("reading datasources.yml")?;
    let have = || all.iter().map(|c| c.uid.as_str()).collect::<Vec<_>>().join(", ");
    match datasource {
        Some(uid) => {
            let want = uid.trim().to_lowercase();
            all.iter()
                .find(|c| c.uid == want)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("datasources.yml names no datasource {uid}; it has {}", have()))
        }
        None if all.len() == 1 => Ok(all[0].clone()),
        None if all.is_empty() => bail!("datasources.yml names no datasource"),
        None => bail!("datasources.yml names several datasources ({}); say which", have()),
    }
}

fn open(project_dir: &Path, conn: &Connection) -> Result<Box<dyn Source>> {
    let secrets = zsql_source::secrets(Some(project_dir), &conn.uid);
    let src = zsql_source::open(conn, secrets).with_context(|| format!("opening datasource {}", conn.uid))?;
    for w in src.warnings() {
        eprintln!("warning: {w}");
    }
    Ok(src)
}

fn load(project_dir: &Path, uid: &str) -> Result<Option<Catalog>> {
    let out = path(project_dir, uid);
    match std::fs::read_to_string(&out) {
        Ok(text) => {
            Ok(Some(serde_json::from_str(&text).with_context(|| format!("{} does not parse; delete it and read again", out.display()))?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| out.display().to_string()),
    }
}

fn write(project_dir: &Path, cat: &Catalog) -> Result<std::path::PathBuf> {
    let out = path(project_dir, &cat.datasource);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A trailing newline keeps diffs clean.
    let mut json = serde_json::to_string_pretty(cat)?;
    json.push('\n');
    std::fs::write(&out, json.as_bytes())?;
    Ok(out)
}

fn say_drift(uid: &str, before: &Catalog, fresh: &Catalog) {
    let drift = Drift::between(before, fresh);
    if !drift.is_empty() {
        println!("{uid}: since the catalog read {}:", before.read_at.get(..10).unwrap_or(&before.read_at));
        for line in drift.lines() {
            println!("  {line}");
        }
    }
}

/// `schema.table` or `table`, as a user names one.
fn split(name: &str) -> (Option<&str>, &str) {
    match name.rsplit_once('.') {
        Some((s, t)) if !s.is_empty() => (Some(s), t),
        _ => (None, name),
    }
}

/// The read, or the sync: every schema named, else every scope and every
/// added table the file already records, else every non-system schema.
pub fn run(project_dir: Option<&Path>, datasource: Option<&str>, schemas: &[String]) -> Result<()> {
    let project = config::find_project(project_dir.unwrap_or(Path::new(".")))?;
    let conn = choose(&project.dir, datasource)?;
    let t0 = Instant::now();
    let mut src = open(&project.dir, &conn)?;
    let before = load(&project.dir, &conn.uid)?;
    let mut merged = before.clone().unwrap_or_default();
    merged.datasource = conn.uid.clone();

    let scopes: Vec<Option<String>> = if !schemas.is_empty() {
        schemas.iter().map(|s| Some(s.trim().to_string())).collect()
    } else if let Some(b) = &before {
        if b.scopes.is_empty() && b.added.is_empty() {
            vec![None]
        } else {
            b.scopes.iter().map(|s| s.schema.clone()).collect()
        }
    } else {
        vec![None]
    };
    let mut failed = false;
    for scope in &scopes {
        match Catalog::read_filtered(src.as_mut(), &conn.uid, &Filter::schema(scope.as_deref())) {
            Ok(fresh) => {
                if let Some(b) = &before {
                    say_drift(&conn.uid, b, &fresh);
                }
                if fresh.tables.is_empty() {
                    eprintln!(
                        "{}: nothing was read{}: check the schema, or the credential's grants",
                        conn.uid,
                        scope.as_deref().map(|s| format!(" in {s}")).unwrap_or_default()
                    );
                }
                merged.merge(fresh);
            }
            Err(e) => {
                failed = true;
                eprintln!(
                    "error: {}: reading {} failed, so what the file holds for it is kept as it was: {e}",
                    conn.uid,
                    scope.as_deref().unwrap_or("every schema")
                );
            }
        }
    }
    // Tables read by name, when no scope just read covers them.
    let whole = scopes.iter().any(|s| s.is_none());
    let added: Vec<(Option<String>, String)> =
        before.as_ref().map(|b| b.added.iter().map(|a| (a.schema.clone(), a.name.clone())).collect()).unwrap_or_default();
    for (schema, name) in added {
        if whole || scopes.iter().any(|s| s.as_deref().map(str::to_lowercase) == schema.as_deref().map(str::to_lowercase)) {
            continue;
        }
        match Catalog::read_filtered(src.as_mut(), &conn.uid, &Filter::table(schema.as_deref(), &name)) {
            Ok(fresh) if fresh.tables.is_empty() => {
                // Asked for by name and not there: dropped, not unread.
                println!("{}: table dropped:  {}{name}", conn.uid, schema.as_deref().map(|s| format!("{s}.")).unwrap_or_default());
                merged.forget(schema.as_deref(), &name);
            }
            Ok(fresh) => {
                if let Some(b) = &before {
                    say_drift(&conn.uid, b, &fresh);
                }
                merged.merge(fresh);
            }
            Err(e) => {
                failed = true;
                eprintln!("error: {}: re-reading {name} failed, so it is kept as it was: {e}", conn.uid);
            }
        }
    }
    let out = write(&project.dir, &merged)?;
    summary(&merged, &out, &project.dir, t0);
    if failed {
        bail!("{}: not every read succeeded; the file keeps what it had for those", conn.uid);
    }
    Ok(())
}

fn summary(cat: &Catalog, out: &Path, project_dir: &Path, t0: Instant) {
    let columns: usize = cat.tables.iter().map(|t| t.columns.len()).sum();
    let views = cat.tables.iter().filter(|t| t.kind == "view").count();
    let mut scope: Vec<String> = cat.scopes.iter().map(|s| s.schema.clone().unwrap_or_else(|| "every schema".into())).collect();
    if !cat.added.is_empty() {
        scope.push(format!("{} added by name", cat.added.len()));
    }
    println!(
        "{}: {} tables, {} views, {} columns ({}) -> {} ({:.0} ms)",
        cat.datasource,
        cat.tables.len() - views,
        views,
        columns,
        scope.join(", "),
        out.strip_prefix(project_dir).unwrap_or(out).display(),
        t0.elapsed().as_secs_f64() * 1e3
    );
}

/// `zsql catalog search <pattern>`: the file, or the warehouse with
/// `--live`, which asks every non-system schema for names holding the
/// pattern and reads those tables whole.
pub fn search(project_dir: Option<&Path>, datasource: Option<&str>, pattern: &str, live: bool) -> Result<()> {
    let project = config::find_project(project_dir.unwrap_or(Path::new(".")))?;
    let conn = choose(&project.dir, datasource)?;
    let want = pattern.trim().to_lowercase();
    if want.is_empty() {
        bail!("give a pattern to search for");
    }
    let (tables, where_) = if live {
        let mut src = open(&project.dir, &conn)?;
        let fresh = Catalog::read_filtered(src.as_mut(), &conn.uid, &Filter::like(None, &want))?;
        (fresh.tables, "the warehouse")
    } else {
        let Some(cat) = load(&project.dir, &conn.uid)? else {
            bail!("{}: no catalog yet; `zsql catalog {}` reads it, or `--live` searches the warehouse", conn.uid, conn.uid);
        };
        let hits = cat.tables.iter().filter(|t| t.physical_name().to_lowercase().contains(&want)).cloned().collect();
        (hits, "the catalog")
    };
    if tables.is_empty() {
        println!("nothing in {where_} matches {pattern}{}", if live { "" } else { "; --live searches the warehouse" });
        return Ok(());
    }
    let held = load(&project.dir, &conn.uid)?.unwrap_or_default();
    for t in &tables {
        let in_file = held.tables.iter().any(|h| {
            h.name.eq_ignore_ascii_case(&t.name) && h.schema.as_deref().map(str::to_lowercase) == t.schema.as_deref().map(str::to_lowercase)
        });
        println!(
            "{:<40} {:<5} {:>4} columns{}{}",
            t.physical_name(),
            t.kind,
            t.columns.len(),
            t.rows.map(|r| format!(", ~{r} rows")).unwrap_or_default(),
            if live && !in_file { "   (not in the catalog; `zsql catalog add <name>` adds it)" } else { "" }
        );
    }
    Ok(())
}

/// `zsql catalog add <schema.table>...`: each read by name, live, and
/// folded into the file with its own provenance.
pub fn add(project_dir: Option<&Path>, datasource: Option<&str>, names: &[String]) -> Result<()> {
    let project = config::find_project(project_dir.unwrap_or(Path::new(".")))?;
    let conn = choose(&project.dir, datasource)?;
    if names.is_empty() {
        bail!("name the tables to add, as schema.table or table");
    }
    let t0 = Instant::now();
    let mut src = open(&project.dir, &conn)?;
    let before = load(&project.dir, &conn.uid)?;
    let mut merged = before.clone().unwrap_or_default();
    merged.datasource = conn.uid.clone();
    let mut missing = Vec::new();
    for name in names {
        let (schema, table) = split(name);
        let fresh =
            Catalog::read_filtered(src.as_mut(), &conn.uid, &Filter::table(schema, table)).with_context(|| format!("reading {name}"))?;
        if fresh.tables.is_empty() {
            missing.push(name.clone());
            continue;
        }
        if let Some(b) = &before {
            say_drift(&conn.uid, b, &fresh);
        }
        for t in &fresh.tables {
            println!("added   {}", t.physical_name());
        }
        merged.merge(fresh);
    }
    let out = write(&project.dir, &merged)?;
    summary(&merged, &out, &project.dir, t0);
    if !missing.is_empty() {
        bail!("{}: the warehouse has no table named {}; `zsql catalog search <pattern> --live` finds names", conn.uid, missing.join(", "));
    }
    Ok(())
}

//! `zsql catalog`: what a datasource holds, in `catalog/<datasource>.json`
//! beside the model. Local only: the server never holds a warehouse
//! credential.
//!
//! The file is a union with its provenance: the schemas read whole, each
//! with when, and the tables read one by one. `read` adds to it, `sync`
//! re-reads every one of those and replaces what each read covers, and
//! neither drops a table on a failed read, only on a read that succeeded
//! and did not find it. Bare `catalog` says what the file holds and
//! connects to nothing. See `docs/introspection.md`.

use crate::{config, tty};
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
pub fn choose(project_dir: &Path, datasource: Option<&str>) -> Result<Connection> {
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
        None if all.is_empty() => {
            bail!("no datasource yet; `zsql datasource add`, or edit datasources.yml (it has an example of every kind)")
        }
        None => bail!("datasources.yml names several datasources ({}); say which with -d", have()),
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

pub fn load(project_dir: &Path, uid: &str) -> Result<Option<Catalog>> {
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
pub fn split(name: &str) -> (Option<&str>, &str) {
    match name.rsplit_once('.') {
        Some((s, t)) if !s.is_empty() => (Some(s), t),
        _ => (None, name),
    }
}

/// The summary line, last and on stderr: what the file holds now.
fn summary(cat: &Catalog, out: &Path, project_dir: &Path, t0: Instant) {
    let columns: usize = cat.tables.iter().map(|t| t.columns.len()).sum();
    let views = cat.tables.iter().filter(|t| t.kind == "view").count();
    let mut scope: Vec<String> = cat.scopes.iter().map(|s| s.schema.clone().unwrap_or_else(|| "every schema".into())).collect();
    if !cat.added.is_empty() {
        scope.push(format!("{} added by name", cat.added.len()));
    }
    eprintln!(
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

/// Bare `zsql catalog`: what the file holds, for one datasource or all.
pub fn status(project_dir: Option<&Path>, datasource: Option<&str>) -> Result<()> {
    let project = config::find_project(project_dir.unwrap_or(Path::new(".")))?;
    let all = Connection::read_all(&project.dir).context("reading datasources.yml")?;
    if all.is_empty() {
        println!("no datasource yet; `zsql datasource add`, or edit datasources.yml");
        return Ok(());
    }
    let chosen: Vec<&Connection> = match datasource {
        Some(d) => vec![all
            .iter()
            .find(|c| c.uid == d.trim().to_lowercase())
            .ok_or_else(|| anyhow::anyhow!("datasources.yml names no datasource {d}"))?],
        None => all.iter().collect(),
    };
    for conn in chosen {
        match load(&project.dir, &conn.uid)? {
            None => println!("{}: no catalog yet; `zsql catalog read -d {} --schema <name>` reads one", conn.uid, conn.uid),
            Some(cat) => {
                let views = cat.tables.iter().filter(|t| t.kind == "view").count();
                let columns: usize = cat.tables.iter().map(|t| t.columns.len()).sum();
                println!(
                    "{}: {} tables, {} views, {} columns; read {}",
                    conn.uid,
                    cat.tables.len() - views,
                    views,
                    columns,
                    cat.read_at.get(..10).unwrap_or(&cat.read_at)
                );
                for s in &cat.scopes {
                    println!(
                        "  schema {:<24} read {}",
                        s.schema.clone().unwrap_or_else(|| "(every schema)".into()),
                        s.read_at.get(..10).unwrap_or("")
                    );
                }
                for a in &cat.added {
                    println!(
                        "  table  {:<24} read {}",
                        format!("{}{}", a.schema.as_deref().map(|s| format!("{s}.")).unwrap_or_default(), a.name),
                        a.read_at.get(..10).unwrap_or("")
                    );
                }
            }
        }
    }
    eprintln!("`zsql catalog sync` re-reads what is recorded; `zsql catalog read --schema <name>` adds to it");
    Ok(())
}

/// `zsql catalog read`: named schemas and tables, read and merged in.
/// With nothing named, a schema picker on a terminal.
pub fn read(project_dir: Option<&Path>, datasource: Option<&str>, schemas: &[String], tables: &[String], all: bool) -> Result<()> {
    let project = config::find_project(project_dir.unwrap_or(Path::new(".")))?;
    let conn = choose(&project.dir, datasource)?;
    let t0 = Instant::now();
    let mut src = open(&project.dir, &conn)?;
    let before = load(&project.dir, &conn.uid)?;
    let mut merged = before.clone().unwrap_or_default();
    merged.datasource = conn.uid.clone();

    let mut scopes: Vec<Option<String>> = schemas.iter().map(|s| Some(s.trim().to_string())).collect();
    if all {
        scopes = vec![None];
    }
    if scopes.is_empty() && tables.is_empty() {
        // The schemas the connection can see, to pick from.
        let flag_form = "say which: --schema NAME (repeatable), table names, or --all";
        if !tty::interactive() {
            bail!("{flag_form}");
        }
        let seen = schemas_of(src.as_mut(), &conn)?;
        if seen.is_empty() {
            bail!("{}: no schemas to pick from; --all reads everything the connection can see", conn.uid);
        }
        let options: Vec<String> = seen.iter().map(|(s, n)| format!("{s:<32} {n} tables")).collect();
        let chosen = tty::pick_many("schemas to read", options, vec![], flag_form)?;
        if chosen.is_empty() {
            return Err(tty::Cancelled("nothing selected; nothing read").into());
        }
        scopes = chosen.into_iter().map(|i| Some(seen[i].0.clone())).collect();
    }
    let mut failed = Vec::new();
    for scope in &scopes {
        match Catalog::read_filtered(src.as_mut(), &conn.uid, &Filter::schema(scope.as_deref())) {
            Ok(fresh) => {
                if let Some(b) = &before {
                    say_drift(&conn.uid, b, &fresh);
                }
                if fresh.tables.is_empty() {
                    eprintln!(
                        "warning: {}: nothing was read{}: check the schema, or the credential's grants",
                        conn.uid,
                        scope.as_deref().map(|s| format!(" in {s}")).unwrap_or_default()
                    );
                }
                merged.merge(fresh);
            }
            Err(e) => failed.push(format!("{}: kept as it was: {e}", scope.as_deref().unwrap_or("every schema"))),
        }
    }
    let mut missing = Vec::new();
    for name in tables {
        let (schema, table) = split(name);
        match Catalog::read_filtered(src.as_mut(), &conn.uid, &Filter::table(schema, table)) {
            Ok(fresh) if fresh.tables.is_empty() => missing.push(name.clone()),
            Ok(fresh) => {
                if let Some(b) = &before {
                    say_drift(&conn.uid, b, &fresh);
                }
                for t in &fresh.tables {
                    println!("added   {}", t.physical_name());
                }
                merged.merge(fresh);
            }
            Err(e) => failed.push(format!("{name}: {e}")),
        }
    }
    for f in &failed {
        eprintln!("warning: {}: {f}", conn.uid);
    }
    let out = write(&project.dir, &merged)?;
    summary(&merged, &out, &project.dir, t0);
    if !missing.is_empty() {
        bail!("{}: the warehouse has no table named {}; `zsql catalog search <pattern> --live` finds names", conn.uid, missing.join(", "));
    }
    if !failed.is_empty() {
        bail!("{}: not every read succeeded; the file keeps what it had for those", conn.uid);
    }
    eprintln!("next: zsql generate");
    Ok(())
}

/// The schemas a connection can see, with a table count each: a read of
/// every schema's table list, which is one statement, not the columns.
fn schemas_of(src: &mut dyn Source, conn: &Connection) -> Result<Vec<(String, usize)>> {
    let cat = Catalog::read_filtered(src, &conn.uid, &Filter::schema(None)).with_context(|| format!("{}: listing schemas", conn.uid))?;
    let mut out: Vec<(String, usize)> = Vec::new();
    for t in &cat.tables {
        let Some(s) = &t.schema else { continue };
        match out.iter_mut().find(|(n, _)| n == s) {
            Some((_, n)) => *n += 1,
            None => out.push((s.clone(), 1)),
        }
    }
    Ok(out)
}

/// `zsql catalog sync`: every scope and every added table the file
/// records, re-read; what a successful read did not find is dropped.
pub fn sync(project_dir: Option<&Path>, datasource: Option<&str>) -> Result<()> {
    let project = config::find_project(project_dir.unwrap_or(Path::new(".")))?;
    let conn = choose(&project.dir, datasource)?;
    let Some(before) = load(&project.dir, &conn.uid)? else {
        bail!("{}: no catalog yet; `zsql catalog read --schema <name>` reads one", conn.uid);
    };
    let t0 = Instant::now();
    let mut src = open(&project.dir, &conn)?;
    let mut merged = before.clone();
    let scopes: Vec<Option<String>> = if before.scopes.is_empty() && before.added.is_empty() {
        vec![None]
    } else {
        before.scopes.iter().map(|s| s.schema.clone()).collect()
    };
    let mut failed = Vec::new();
    for scope in &scopes {
        match Catalog::read_filtered(src.as_mut(), &conn.uid, &Filter::schema(scope.as_deref())) {
            Ok(fresh) => {
                say_drift(&conn.uid, &before, &fresh);
                merged.merge(fresh);
            }
            Err(e) => failed.push(format!("{}: kept as it was: {e}", scope.as_deref().unwrap_or("every schema"))),
        }
    }
    let whole = scopes.iter().any(|s| s.is_none());
    for a in &before.added {
        if whole || scopes.iter().any(|s| s.as_deref().map(str::to_lowercase) == a.schema.as_deref().map(str::to_lowercase)) {
            continue;
        }
        match Catalog::read_filtered(src.as_mut(), &conn.uid, &Filter::table(a.schema.as_deref(), &a.name)) {
            Ok(fresh) if fresh.tables.is_empty() => {
                // Asked for by name and not there: dropped, not unread.
                println!("{}: table dropped:  {}{}", conn.uid, a.schema.as_deref().map(|s| format!("{s}.")).unwrap_or_default(), a.name);
                merged.forget(a.schema.as_deref(), &a.name);
            }
            Ok(fresh) => {
                say_drift(&conn.uid, &before, &fresh);
                merged.merge(fresh);
            }
            Err(e) => failed.push(format!("{}: kept as it was: {e}", a.name)),
        }
    }
    for f in &failed {
        eprintln!("warning: {}: {f}", conn.uid);
    }
    let out = write(&project.dir, &merged)?;
    summary(&merged, &out, &project.dir, t0);
    if !failed.is_empty() {
        bail!("{}: not every read succeeded; the file keeps what it had for those", conn.uid);
    }
    Ok(())
}

/// `zsql catalog search <pattern>`: the file, or the warehouse with
/// `--live`, which asks every non-system schema for names holding the
/// pattern and reads those tables whole.
pub fn search(project_dir: Option<&Path>, datasource: Option<&str>, pattern: &str, live: bool, json: bool) -> Result<()> {
    let project = config::find_project(project_dir.unwrap_or(Path::new(".")))?;
    let conn = choose(&project.dir, datasource)?;
    let want = pattern.trim().to_lowercase();
    let held = load(&project.dir, &conn.uid)?;
    let (tables, where_) = if live {
        let mut src = open(&project.dir, &conn)?;
        let fresh = Catalog::read_filtered(src.as_mut(), &conn.uid, &Filter::like(None, &want))?;
        (fresh.tables, "the warehouse")
    } else {
        let Some(cat) = &held else {
            bail!("{}: no catalog yet; `zsql catalog read --schema <name>` reads one, or `--live` searches the warehouse", conn.uid);
        };
        let hits = cat.tables.iter().filter(|t| t.physical_name().to_lowercase().contains(&want)).cloned().collect();
        (hits, "the catalog")
    };
    let in_file = |t: &zsql_source::Table| {
        held.as_ref().is_some_and(|h| {
            h.tables.iter().any(|x| {
                x.name.eq_ignore_ascii_case(&t.name)
                    && x.schema.as_deref().map(str::to_lowercase) == t.schema.as_deref().map(str::to_lowercase)
            })
        })
    };
    if json {
        let v: Vec<serde_json::Value> = tables
            .iter()
            .map(|t| {
                serde_json::json!({
                    "schema": t.schema, "name": t.name, "physical_name": t.physical_name(), "kind": t.kind,
                    "columns": t.columns.len(), "rows": t.rows, "in_catalog": in_file(t),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "datasource": conn.uid, "live": live, "tables": v }))?);
        return Ok(());
    }
    if tables.is_empty() {
        println!("nothing in {where_} matches {pattern}{}", if live { "" } else { "; --live searches the warehouse" });
        return Ok(());
    }
    for t in &tables {
        println!(
            "{:<40} {:<5} {:>4} columns{}{}",
            t.physical_name(),
            t.kind,
            t.columns.len(),
            t.rows.map(|r| format!(", ~{r} rows")).unwrap_or_default(),
            if live && !in_file(t) { "   (not in the catalog; `zsql catalog read <name>` adds it)" } else { "" }
        );
    }
    Ok(())
}

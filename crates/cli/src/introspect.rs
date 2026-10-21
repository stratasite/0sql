//! `zsql introspect`: read a datasource's catalogue and write it beside
//! the model. Local only: the server never holds a warehouse credential.

use crate::config;
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::time::Instant;
use zsql_source::{Catalogue, Connection};

/// Where a datasource's catalogue is written: one file per datasource,
/// so re-reading one never rewrites another, and committed rather than
/// gitignored, because it is schema and a diff in review is where drift
/// wants to be seen.
pub fn path(project_dir: &Path, uid: &str) -> std::path::PathBuf {
    project_dir.join("catalogue").join(format!("{uid}.json"))
}

pub fn run(project_dir: Option<&Path>, datasource: Option<&str>, schema: Option<&str>) -> Result<()> {
    let project = config::find_project(project_dir.unwrap_or(Path::new(".")))?;
    let all = Connection::read_all(&project.dir).context("reading datasources.yml")?;
    let chosen: Vec<&Connection> = match datasource {
        Some(uid) => {
            let want = uid.trim().to_lowercase();
            match all.iter().find(|c| c.uid == want) {
                Some(c) => vec![c],
                None => {
                    let have: Vec<&str> = all.iter().map(|c| c.uid.as_str()).collect();
                    bail!("datasources.yml names no datasource {uid}; it has {}", have.join(", "));
                }
            }
        }
        None if all.len() == 1 => all.iter().collect(),
        None => {
            let have: Vec<&str> = all.iter().map(|c| c.uid.as_str()).collect();
            bail!("datasources.yml names several datasources ({}); say which", have.join(", "));
        }
    };

    for conn in chosen {
        let t0 = Instant::now();
        let secret = zsql_source::secret(Some(&project.dir), &conn.uid, "password");
        let mut src = zsql_source::open(conn, secret).with_context(|| format!("opening datasource {}", conn.uid))?;
        let scope = schema.map(str::to_string).or_else(|| conn.schema.clone()).or_else(|| {
            // ClickHouse's database is its schema; Postgres's database holds
            // schemas and the scope is the schema alone.
            (conn.adapter == "clickhouse").then(|| conn.database.clone()).flatten()
        });
        let cat = Catalogue::read(src.as_mut(), &conn.uid, scope.as_deref()).with_context(|| format!("reading {}", conn.uid))?;

        let out = path(&project.dir, &conn.uid);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&cat)?;
        std::fs::write(&out, json.as_bytes() /* a trailing newline keeps diffs clean */)?;
        std::fs::OpenOptions::new().append(true).open(&out)?.write_all(b"\n")?;

        let columns: usize = cat.tables.iter().map(|t| t.columns.len()).sum();
        let views = cat.tables.iter().filter(|t| t.kind == "view").count();
        println!(
            "{}: {} tables, {} views, {} columns{} -> {} ({:.0} ms)",
            conn.uid,
            cat.tables.len() - views,
            views,
            columns,
            match &scope {
                Some(s) => format!(" in {s}"),
                None => String::new(),
            },
            out.strip_prefix(&project.dir).unwrap_or(&out).display(),
            t0.elapsed().as_secs_f64() * 1e3
        );
        if cat.tables.is_empty() {
            eprintln!("nothing was read: check the schema, or the credential's grants");
        }
    }
    Ok(())
}

use std::io::Write;

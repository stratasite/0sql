//! `zsql datasource`: the warehouses in `datasources.yml`. `list` says
//! what is there and whether each has its secret, without connecting;
//! `add` writes an entry and its secret, asking on a terminal for the
//! keys the adapter needs and taking them as flags otherwise; `test`
//! opens the connection and says what it can see.

use crate::{config, tty};
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use std::path::Path;
use zsql_source::{Catalog, Connection, Filter};

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum DatasourceWhat {
    /// The datasources in datasources.yml: key, adapter, tier, and whether a secret is at hand.
    List,
    /// Add a datasource: the entry to datasources.yml, the secret to .zsql. Asks on a terminal; flags say it otherwise.
    Add {
        /// The key other files refer to it by.
        key: Option<String>,
        /// The adapter: postgres, mysql, sqlserver, clickhouse, trino, sqlite, duckdb, snowflake, databricks, bigquery, athena, redshift.
        #[arg(long)]
        adapter: Option<String>,
        /// hot, warm or cold; hot when unsaid.
        #[arg(long)]
        tier: Option<String>,
        /// The display name; the key when unsaid.
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        schema: Option<String>,
        #[arg(long)]
        username: Option<String>,
        /// For sqlite and duckdb: the file.
        #[arg(long)]
        file: Option<String>,
        /// Snowflake's account identifier.
        #[arg(long)]
        account: Option<String>,
        /// Snowflake's warehouse, or Databricks' SQL warehouse id.
        #[arg(long)]
        warehouse: Option<String>,
        /// Databricks' catalog, or Athena's data catalog.
        #[arg(long)]
        catalog: Option<String>,
        /// BigQuery's project.
        #[arg(long)]
        project_id: Option<String>,
        /// BigQuery's service-account key file.
        #[arg(long)]
        key_file: Option<String>,
        /// Athena's region.
        #[arg(long)]
        region: Option<String>,
        /// Athena's result location on S3.
        #[arg(long)]
        output_location: Option<String>,
        /// Read the password or token from stdin, for scripts.
        #[arg(long)]
        secret_stdin: bool,
    },
    /// Set or replace a datasource's secret in .zsql: the password, token or key its adapter takes.
    Auth {
        /// Which datasource, by its key in datasources.yml; the only one when there is one.
        #[arg(short = 'd', long)]
        datasource: Option<String>,
        /// Which secret: password, personal_access_token, private_key, oauth_client_secret, access_key_id, secret_access_key, session_token. The adapter's usual one when unsaid.
        #[arg(long)]
        field: Option<String>,
        /// Read the secret from stdin, for scripts.
        #[arg(long)]
        secret_stdin: bool,
    },
    /// Open the connection, run a one-row probe, and list the schemas it can see.
    Test {
        /// Which datasource, by its key in datasources.yml; the only one when there is one.
        #[arg(short = 'd', long)]
        datasource: Option<String>,
    },
}

/// Which keys each adapter wants, in the order to ask for them, and
/// which secret field it takes.
fn shape(adapter: &str) -> (&'static [&'static str], Option<&'static str>) {
    match adapter {
        "postgres" | "redshift" => (&["host", "port", "database", "username", "schema"], Some("password")),
        "mysql" => (&["host", "port", "database", "username"], Some("password")),
        "sqlserver" => (&["host", "port", "database", "username", "schema"], Some("password")),
        "clickhouse" => (&["host", "port", "database", "username"], Some("password")),
        "trino" => (&["host", "port", "database", "schema", "username"], Some("password")),
        "sqlite" | "duckdb" => (&["file"], None),
        "snowflake" => (&["account", "database", "warehouse", "schema"], Some("personal_access_token")),
        "databricks" => (&["host", "warehouse", "catalog", "schema"], Some("personal_access_token")),
        "bigquery" => (&["project_id", "schema", "key_file"], None),
        "athena" => (&["region", "database", "output_location"], None),
        _ => (&[], None),
    }
}

const ADAPTERS: [&str; 12] = [
    "postgres",
    "mysql",
    "sqlserver",
    "clickhouse",
    "trino",
    "sqlite",
    "duckdb",
    "snowflake",
    "databricks",
    "bigquery",
    "athena",
    "redshift",
];

pub fn run(project_dir: Option<&Path>, what: DatasourceWhat) -> Result<()> {
    let project = config::find_project(project_dir.unwrap_or(Path::new(".")))?;
    match what {
        DatasourceWhat::List => list(&project.dir),
        DatasourceWhat::Test { datasource } => test(&project.dir, datasource.as_deref()),
        DatasourceWhat::Auth { datasource, field, secret_stdin } => {
            auth(&project.dir, datasource.as_deref(), field.as_deref(), secret_stdin)
        }
        DatasourceWhat::Add {
            key,
            adapter,
            tier,
            name,
            host,
            port,
            database,
            schema,
            username,
            file,
            account,
            warehouse,
            catalog,
            project_id,
            key_file,
            region,
            output_location,
            secret_stdin,
        } => {
            let given: Vec<(&str, Option<String>)> = vec![
                ("host", host),
                ("port", port.map(|p| p.to_string())),
                ("database", database),
                ("schema", schema),
                ("username", username),
                ("file", file),
                ("account", account),
                ("warehouse", warehouse),
                ("catalog", catalog),
                ("project_id", project_id),
                ("key_file", key_file),
                ("region", region),
                ("output_location", output_location),
            ];
            add(&project.dir, key, adapter, tier, name, given, secret_stdin)
        }
    }
}

fn list(dir: &Path) -> Result<()> {
    let all = Connection::read_all(dir).context("reading datasources.yml")?;
    if all.is_empty() {
        println!("no datasource yet; `zsql datasource add`, or edit datasources.yml (it has an example of every kind)");
        return Ok(());
    }
    println!("{:<16} {:<12} {:<6} {:<32} secret", "key", "adapter", "tier", "where");
    for c in &all {
        let secrets = zsql_source::secrets(Some(dir), &c.uid);
        let has = secrets.password.is_some()
            || secrets.personal_access_token.is_some()
            || secrets.private_key.is_some()
            || secrets.oauth_client_secret.is_some()
            || secrets.secret_access_key.is_some()
            || c.private_key_file.is_some()
            || c.key_file.is_some();
        let needs = shape(&c.adapter).1.is_some() || matches!(c.adapter.as_str(), "bigquery" | "athena");
        let where_ = c
            .file
            .as_ref()
            .map(|f| f.display().to_string())
            .or_else(|| c.host.clone())
            .or_else(|| c.account.clone())
            .or_else(|| c.project.clone())
            .or_else(|| c.url.clone())
            .unwrap_or_default();
        let tier = tier_of(dir, &c.uid).unwrap_or_default();
        println!(
            "{:<16} {:<12} {:<6} {:<32} {}",
            c.uid,
            c.adapter,
            tier,
            where_,
            if !needs {
                "none needed"
            } else if has {
                "yes"
            } else {
                "missing: .zsql or ZSQL_DS_*"
            }
        );
    }
    Ok(())
}

fn tier_of(dir: &Path, uid: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("datasources.yml")).ok()?;
    let v: serde_yaml::Value = serde_yaml::from_str(&text).ok()?;
    v.get(uid)?.get("tier")?.as_str().map(str::to_string)
}

fn auth(dir: &Path, datasource: Option<&str>, field: Option<&str>, secret_stdin: bool) -> Result<()> {
    let conn = crate::catalog::choose(dir, datasource)?;
    let usual = shape(&conn.adapter).1.or(if conn.adapter == "athena" { Some("secret_access_key") } else { None });
    let field = match (field, usual) {
        (Some(f), _) => f.to_string(),
        (None, Some(f)) => f.to_string(),
        (None, None) => bail!(
            "{}: {} takes no secret in .zsql ({})",
            conn.uid,
            conn.adapter,
            if conn.adapter == "bigquery" { "its key file is named by key_file in datasources.yml" } else { "it opens a file" }
        ),
    };
    if !zsql_source::SECRET_FIELDS.contains(&field.as_str()) {
        bail!("{field} is not a secret field; one of {}", zsql_source::SECRET_FIELDS.join(", "));
    }
    let value = if secret_stdin {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        s.trim_end_matches(['\n', '\r']).to_string()
    } else {
        tty::ask_secret(&format!("{field} for {}", conn.uid), "--secret-stdin reads it from stdin")?
    };
    if value.is_empty() {
        bail!("nothing entered; nothing saved");
    }
    config::write_secret(dir, &format!("{}_{field}", conn.uid), &value)?;
    println!("saved   {field} for {} in .zsql", conn.uid);
    if field == "access_key_id" {
        eprintln!("next: zsql datasource auth -d {} --field secret_access_key", conn.uid);
    } else {
        eprintln!("next: zsql datasource test -d {}", conn.uid);
    }
    Ok(())
}

fn test(dir: &Path, datasource: Option<&str>) -> Result<()> {
    let conn = crate::catalog::choose(dir, datasource)?;
    let secrets = zsql_source::secrets(Some(dir), &conn.uid);
    let t0 = std::time::Instant::now();
    let mut src = zsql_source::open(&conn, secrets).with_context(|| format!("opening datasource {}", conn.uid))?;
    for w in src.warnings() {
        eprintln!("warning: {w}");
    }
    src.query("SELECT 1 AS one", 1).with_context(|| format!("{}: the connection opened but a statement did not run", conn.uid))?;
    // The schemas it can see: a read of every schema's tables, counted.
    let cat =
        Catalog::read_filtered(src.as_mut(), &conn.uid, &Filter::schema(None)).with_context(|| format!("{}: listing schemas", conn.uid))?;
    let mut schemas: Vec<(String, usize)> = Vec::new();
    for t in &cat.tables {
        let s = t.schema.clone().unwrap_or_else(|| "(no schema)".into());
        match schemas.iter_mut().find(|(n, _)| *n == s) {
            Some((_, n)) => *n += 1,
            None => schemas.push((s, 1)),
        }
    }
    let said: Vec<String> = schemas.iter().map(|(s, n)| format!("{s} ({n})")).collect();
    println!(
        "{}: ok, {} ({:.0} ms); {} schema{}: {}",
        conn.uid,
        conn.adapter,
        t0.elapsed().as_secs_f64() * 1e3,
        schemas.len(),
        if schemas.len() == 1 { "" } else { "s" },
        said.join(", ")
    );
    eprintln!("next: zsql catalog read --schema <name>");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add(
    dir: &Path,
    key: Option<String>,
    adapter: Option<String>,
    tier: Option<String>,
    name: Option<String>,
    given: Vec<(&str, Option<String>)>,
    secret_stdin: bool,
) -> Result<()> {
    let flags = "zsql datasource add KEY --adapter A, with the keys the adapter needs as flags (zsql datasource add --help)";
    let key = match key {
        Some(k) => k,
        None => tty::ask("key (what other files refer to it by)", None, flags)?,
    };
    let key = key.trim().to_lowercase();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        bail!("a key is letters, digits, dashes and underscores");
    }
    if Connection::read_all(dir).unwrap_or_default().iter().any(|c| c.uid == key) {
        bail!("datasources.yml already has {key}; edit it, or pick another key");
    }
    let adapter = match adapter {
        Some(a) => a.trim().to_lowercase(),
        None => {
            let i = tty::pick_one("adapter", ADAPTERS.iter().map(|a| a.to_string()).collect(), flags)?;
            ADAPTERS[i].to_string()
        }
    };
    if !ADAPTERS.contains(&adapter.as_str()) {
        bail!("adapter {adapter} is not one of {}", ADAPTERS.join(", "));
    }
    let (keys, secret_field) = shape(&adapter);
    let mut values: Vec<(String, String)> = Vec::new();
    for k in keys {
        let v = match given.iter().find(|(g, _)| g == k).and_then(|(_, v)| v.clone()) {
            Some(v) => v,
            None => {
                let default = match *k {
                    "port" => match adapter.as_str() {
                        "postgres" => Some("5432"),
                        "redshift" => Some("5439"),
                        "mysql" => Some("3306"),
                        "sqlserver" => Some("1433"),
                        "clickhouse" => Some("8123"),
                        "trino" => Some("8080"),
                        _ => None,
                    },
                    "schema" => Some(""),
                    "warehouse" | "catalog" => Some(""),
                    _ => None,
                };
                tty::ask(k, default, &format!("--{k} is needed for {adapter}; {flags}"))?
            }
        };
        if !v.trim().is_empty() {
            values.push((k.to_string(), v.trim().to_string()));
        }
    }
    let tier = tier.unwrap_or_else(|| "hot".into()).to_lowercase();
    if !matches!(tier.as_str(), "hot" | "warm" | "cold") {
        bail!("tier is hot, warm or cold");
    }
    let name = name.unwrap_or_else(|| key.clone());
    let secret = match secret_field {
        None => None,
        Some(field) => {
            let v = if secret_stdin {
                let mut s = String::new();
                std::io::stdin().read_line(&mut s)?;
                s.trim_end_matches(['\n', '\r']).to_string()
            } else {
                tty::ask_secret(
                    &format!("{field} (kept in .zsql, never deployed; leave empty to add later)"),
                    "--secret-stdin reads it from stdin",
                )?
            };
            (!v.is_empty()).then_some((field, v))
        }
    };
    // The entry, appended.
    let mut entry = format!("\n{key}:\n  name: {}\n  adapter: {adapter}\n  tier: {tier}\n", yaml(&name));
    for (k, v) in &values {
        let k = if *k == "project_id" { "project" } else { k };
        entry.push_str(&format!("  {k}: {}\n", yaml(v)));
    }
    let path = dir.join("datasources.yml");
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&entry);
    std::fs::write(&path, text).with_context(|| path.display().to_string())?;
    println!("added   {key} ({adapter}) to datasources.yml");
    if let Some((field, value)) = secret {
        config::write_secret(dir, &format!("{key}_{field}"), &value)?;
        println!("saved   {field} for {key} in .zsql");
    } else if let Some(field) = secret_field {
        println!("note    no {field} yet: put it in .zsql under {key}, or in {}", zsql_source::secret_env_var(&key, field));
    }
    eprintln!("next: zsql datasource test -d {key}, then zsql catalog read -d {key} --schema <name>");
    Ok(())
}

fn yaml(s: &str) -> String {
    if s.is_empty() || s.contains([':', '#', '\'', '"', '\n']) || s.trim() != s || s.parse::<f64>().is_ok() {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

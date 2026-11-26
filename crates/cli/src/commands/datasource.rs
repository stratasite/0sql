//! `zsql datasource`: the warehouses in `datasources.yml`. `list` says
//! what is there and whether each has its secret, without connecting;
//! `add` writes an entry and its secret, asking on a terminal for the
//! keys the adapter needs and taking them as flags otherwise; `test`
//! opens the connection and says what it can see.

use crate::{config, tty};
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use std::path::Path;
use zsql_datasource::{Catalog, Connection, Filter};

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum DatasourceWhat {
    /// The datasources in datasources.yml: key, adapter, tier, and whether a secret is at hand.
    List {
        /// Print them as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Add a datasource: the entry to datasources.yml, the secret to .zsql. Asks on a terminal; flags say it otherwise.
    Add {
        /// The key other files refer to it by.
        key: Option<String>,
        /// The adapter: postgres, mysql, sqlserver, clickhouse, trino, druid, sqlite, duckdb, snowflake, databricks, bigquery, athena, redshift.
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
        /// BigQuery's service-account JSON key file; or leave it out and save the key with `zsql datasource auth`.
        #[arg(long)]
        key_file: Option<String>,
        /// Athena's region.
        #[arg(long)]
        region: Option<String>,
        /// Athena's result location on S3.
        #[arg(long)]
        output_location: Option<String>,
        /// Read the password or token from stdin, all of it, for scripts.
        #[arg(long)]
        secret_stdin: bool,
    },
    /// Set or replace a datasource's secret in .zsql: the password, token or key its adapter takes.
    Auth {
        /// Which datasource, by its key in datasources.yml; the only one when there is one.
        #[arg(short = 'd', long)]
        datasource: Option<String>,
        /// Which one, of those the adapter takes. Most: password. snowflake: personal_access_token, private_key. databricks: personal_access_token, oauth_client_id, oauth_client_secret. bigquery: private_key, client_email (a service account's). athena: access_key_id, secret_access_key, session_token. The adapter's usual one when unsaid.
        #[arg(long)]
        field: Option<String>,
        /// Read the secret from stdin, all of it, so a PEM key arrives whole. For scripts.
        #[arg(long)]
        secret_stdin: bool,
    },
    /// Open the connection, run a one-row probe, and list the schemas it can see.
    Test {
        /// Which datasource, by its key in datasources.yml; the only one when there is one.
        #[arg(short = 'd', long)]
        datasource: Option<String>,
    },
    /// The ADBC drivers this zsql can load for Snowflake and BigQuery. With one, a kept result is typed and read faster; without, REST answers as text. Needs no project.
    Drivers {
        /// Print them as JSON.
        #[arg(long)]
        json: bool,
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
        // Many clusters run without an authenticator; username is asked
        // for and may be left empty.
        "druid" => (&["host", "port", "schema", "username"], Some("password")),
        "sqlite" | "duckdb" => (&["file"], None),
        "snowflake" => (&["account", "database", "warehouse", "schema"], Some("personal_access_token")),
        "databricks" => (&["host", "warehouse", "catalog", "schema"], Some("personal_access_token")),
        "bigquery" => (&["project_id", "schema", "key_file"], None),
        "athena" => (&["region", "database", "output_location"], None),
        _ => (&[], None),
    }
}

/// What `datasource auth` saves for an adapter: the secrets it logs in
/// with, and for BigQuery the address its key signs for. Nothing for
/// one that opens a file.
fn takes(adapter: &str) -> &'static [&'static str] {
    match adapter {
        "postgres" | "redshift" | "mysql" | "clickhouse" => &["password"],
        "sqlserver" | "trino" | "druid" => &["password", "oauth_token"],
        "snowflake" => &["personal_access_token", "private_key", "oauth_token"],
        "databricks" => &["personal_access_token", "oauth_client_id", "oauth_client_secret", "oauth_token"],
        "bigquery" => &["private_key", "client_email", "oauth_token"],
        "athena" => &["secret_access_key", "access_key_id", "session_token"],
        _ => &[],
    }
}

/// Everything on stdin, less the line ending a shell adds: a PEM key is
/// many lines and all of them are the secret.
fn secret_from_stdin() -> Result<String> {
    let mut s = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
    Ok(s.trim().to_string())
}

const ADAPTERS: [&str; 13] = [
    "postgres",
    "mysql",
    "sqlserver",
    "clickhouse",
    "trino",
    "druid",
    "sqlite",
    "duckdb",
    "snowflake",
    "databricks",
    "bigquery",
    "athena",
    "redshift",
];

pub fn run(project_dir: Option<&Path>, what: DatasourceWhat) -> Result<()> {
    if let DatasourceWhat::Drivers { json } = what {
        return drivers(json || crate::tty::agent());
    }
    let project = config::find_project(project_dir.unwrap_or(Path::new(".")))?;
    match what {
        DatasourceWhat::List { json } => list(&project.dir, json || crate::tty::agent()),
        DatasourceWhat::Test { datasource } => test(&project.dir, datasource.as_deref()),
        DatasourceWhat::Drivers { .. } => unreachable!("answered above, without a project"),
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

fn list(dir: &Path, json: bool) -> Result<()> {
    let all = Connection::read_all(dir).context("reading datasources.yml")?;
    let tiers = tiers(dir);
    if json {
        let rows: Vec<serde_json::Value> = all
            .iter()
            .map(|c| {
                serde_json::json!({
                    "datasource": c.uid,
                    "adapter": c.adapter,
                    "tier": tier_of(&tiers, &c.uid),
                    "where": where_of(c),
                    "secret": secret_state(dir, c),
                })
            })
            .collect();
        return crate::show::print_json(&serde_json::json!({ "datasources": rows }));
    }
    if all.is_empty() {
        println!("no datasource yet; `zsql datasource add`, or edit datasources.yml (it has an example of every kind)");
        return Ok(());
    }
    println!("{:<16} {:<12} {:<6} {:<32} secret", "key", "adapter", "tier", "where");
    for c in &all {
        println!(
            "{:<16} {:<12} {:<6} {:<32} {}",
            c.uid,
            c.adapter,
            tier_of(&tiers, &c.uid).unwrap_or_default(),
            where_of(c),
            secret_state(dir, c)
        );
    }
    Ok(())
}

/// Where a datasource is, in one word: the file, host, account, project or
/// url, whichever it has.
fn where_of(c: &Connection) -> String {
    c.file
        .as_ref()
        .map(|f| f.display().to_string())
        .or_else(|| c.host.clone())
        .or_else(|| c.account.clone())
        .or_else(|| c.project.clone())
        .or_else(|| c.url.clone())
        .unwrap_or_default()
}

/// Whether what this adapter logs in with is at hand: the same look
/// the connection takes, so `yes` here is not `missing` there.
fn secret_state(dir: &Path, c: &Connection) -> &'static str {
    let secrets = zsql_datasource::secrets(Some(dir), &c.uid);
    match zsql_datasource::credentials_at_hand(c, &secrets) {
        None => "none needed",
        Some(true) => "yes",
        Some(false) if c.adapter == "bigquery" => "missing: .zsql, key_file or gcloud",
        Some(false) => "missing: .zsql or ZSQL_DS_*",
    }
}

/// The tiers `datasources.yml` states, by datasource key. Read once: a
/// listing asked per datasource and re-parsed the whole file each time.
fn tiers(dir: &Path) -> serde_yaml::Value {
    std::fs::read_to_string(dir.join("datasources.yml"))
        .ok()
        .and_then(|text| serde_yaml::from_str(&text).ok())
        .unwrap_or(serde_yaml::Value::Null)
}

fn tier_of(all: &serde_yaml::Value, uid: &str) -> Option<String> {
    all.get(uid)?.get("tier")?.as_str().map(str::to_string)
}

fn auth(dir: &Path, datasource: Option<&str>, field: Option<&str>, secret_stdin: bool) -> Result<()> {
    let conn = crate::commands::catalog::choose(dir, datasource)?;
    let takes = takes(&conn.adapter);
    let Some(usual) = takes.first() else {
        bail!("{}: {} takes no secret in .zsql (it opens a file)", conn.uid, conn.adapter);
    };
    let field = field.unwrap_or(usual).trim().to_lowercase();
    if !takes.contains(&field.as_str()) {
        bail!("{}: {} does not take {field}; it takes {}", conn.uid, conn.adapter, takes.join(", "));
    }
    let value = if secret_stdin {
        secret_from_stdin()?
    } else if field == "client_email" {
        tty::ask(&format!("{field} for {}", conn.uid), None, "--secret-stdin reads it from stdin")?.trim().to_string()
    } else {
        let what = if field == "private_key" {
            "private_key (the path of its PEM file, which is read now; --secret-stdin takes the PEM itself)"
        } else {
            field.as_str()
        };
        tty::ask_secret(&format!("{what} for {}", conn.uid), "--secret-stdin reads it from stdin")?.trim().to_string()
    };
    if value.is_empty() {
        bail!("nothing entered; nothing saved");
    }
    // A key named by its file is saved as the key: a path means one
    // thing from this directory and another from the next, and nothing
    // at all to a server a deploy carries it to.
    let value = if field == "private_key" && !value.contains("-----BEGIN") {
        let pem = std::fs::read_to_string(&value).with_context(|| format!("{value}: neither a PEM nor a file that can be read"))?;
        if !pem.contains("-----BEGIN") {
            bail!("{value} is not a PEM; a service account's JSON key file is named by key_file in datasources.yml");
        }
        pem.trim().to_string()
    } else {
        value
    };
    config::write_secret(dir, &format!("{}_{field}", conn.uid), &value)?;
    println!("saved   {field} for {} in .zsql", conn.uid);
    // What a connection would now read, which is what was just saved
    // unless something nearer holds another.
    let read = zsql_datasource::local_entry(Some(dir), &conn.uid).get(field.as_str()).and_then(|v| v.as_str()).map(str::to_string);
    if read.as_deref() != Some(value.as_str()) {
        eprintln!("warning: .zsql holds another {field} nested under `{}:`, and that one is read first; remove it there", conn.uid);
    }
    let after = Connection::read(dir, &conn.uid).unwrap_or(conn);
    let secrets = zsql_datasource::secrets(Some(dir), &after.uid);
    let next = match (after.adapter.as_str(), field.as_str()) {
        ("athena", "access_key_id") => Some("secret_access_key"),
        ("bigquery", "private_key") if after.client_email.is_none() => Some("client_email"),
        ("bigquery", "client_email") if secrets.private_key.is_none() => Some("private_key"),
        ("snowflake", "private_key") if after.username.is_none() => {
            eprintln!("note: a key pair signs for a user; say which as username in datasources.yml");
            None
        }
        _ => None,
    };
    match next {
        Some(field) => eprintln!("next: zsql datasource auth -d {} --field {field}", after.uid),
        None => eprintln!("next: zsql datasource test -d {}", after.uid),
    }
    Ok(())
}

/// What `dbc install` has put where this build can load it. A driver
/// that is installed and will not load fails the command: that is a
/// broken install, and `zsql run` would refuse on it too.
fn drivers(json: bool) -> Result<()> {
    use zsql_datasource::Driver;
    let found = zsql_datasource::drivers();
    if json {
        let list: Vec<serde_json::Value> = found
            .iter()
            .flatten()
            .map(|(name, d)| {
                let (state, error) = match d {
                    Driver::Loaded => ("loaded", None),
                    Driver::Missing => ("missing", None),
                    Driver::Broken(e) => ("broken", Some(e.as_str())),
                };
                serde_json::json!({ "driver": name, "state": state, "error": error })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "can_load": found.is_some(), "drivers": list }))?);
    } else {
        match &found {
            None => println!("no drivers: this build cannot load one, or ZSQL_ADBC switches them off. REST answers, as text."),
            Some(list) => {
                for (name, d) in list {
                    match d {
                        Driver::Loaded => println!("{name}: loaded"),
                        Driver::Missing => println!("{name}: not installed (dbc install {name})"),
                        Driver::Broken(e) => println!("{name}: installed and will not load: {e}"),
                    }
                }
                if list.iter().any(|(_, d)| *d == Driver::Missing) {
                    eprintln!("dbc is Columnar's driver installer: https://docs.columnar.tech/dbc/");
                }
            }
        }
    }
    if found.iter().flatten().any(|(_, d)| matches!(d, Driver::Broken(_))) {
        bail!("a driver is installed and will not load");
    }
    Ok(())
}

fn test(dir: &Path, datasource: Option<&str>) -> Result<()> {
    let conn = crate::commands::catalog::choose(dir, datasource)?;
    let secrets = zsql_datasource::secrets(Some(dir), &conn.uid);
    let t0 = std::time::Instant::now();
    let mut src = zsql_datasource::open(&conn, secrets).with_context(|| format!("opening datasource {}", conn.uid))?;
    for w in src.warnings() {
        eprintln!("warning: {w}");
    }
    // `x`, not `one`: Druid reserves that word.
    src.query("SELECT 1 AS x", 1).with_context(|| format!("{}: the connection opened but a statement did not run", conn.uid))?;
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
                        "druid" => Some("8888"),
                        _ => None,
                    },
                    "schema" => Some(""),
                    "username" if adapter == "druid" => Some(""),
                    "warehouse" | "catalog" | "key_file" => Some(""),
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
                secret_from_stdin()?
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
    } else if let Some(field) = secret_field.filter(|_| adapter != "druid" || values.iter().any(|(k, _)| k == "username")) {
        // A Druid with no username is a cluster without an authenticator,
        // and a password is not missing from it.
        println!("note    no {field} yet: put it in .zsql under {key}, or in {}", zsql_datasource::secret_env_var(&key, field));
    }
    if adapter == "bigquery" && !values.iter().any(|(k, _)| k == "key_file") {
        println!("note    no key_file: `zsql datasource auth -d {key}` saves a service account's private_key and client_email in .zsql, or gcloud's own login is used");
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

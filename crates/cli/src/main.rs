//! zsql: author a semantic model, deploy it, query it. No SQL is generated
//! here; every plan comes from a zsqld server.

mod config;
use zsql_shorthand as exprs;
mod remote;
mod templates;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use config::Project;
use remote::Client;
use serde_json::{json, Value};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "zsql", about = "zsql: semantic layer to SQL (0sql.io)", version)]
struct Cli {
    /// Without a command, `zsql` inside a project opens the repl.
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    at: Where,
}

/// Which project and branch a command works on, and which server.
#[derive(Args, Clone, Default)]
struct Where {
    /// Project directory; defaults to the one holding project.yml here or above.
    #[arg(long, global = true)]
    project: Option<PathBuf>,
    /// Branch; defaults to the checked-out git branch.
    #[arg(long, global = true)]
    branch: Option<String>,
    /// The zsqld server; defaults to ZSQL_SERVER, .zsql, project.yml, then the local server.
    #[arg(long, global = true)]
    server: Option<String>,
    /// Project uid, when there is no project directory at hand.
    #[arg(long, global = true)]
    uid: Option<String>,
}

struct Target {
    project: Option<Project>,
    uid: String,
    branch: String,
    client: Client,
}

impl Where {
    fn resolve(&self) -> Result<Target> {
        let project = match &self.project {
            Some(dir) => Some(config::read_project(dir)?),
            None => match (&self.uid, config::find_project(Path::new("."))) {
                (_, Ok(p)) => Some(p),
                (Some(_), Err(_)) => None,
                (None, Err(e)) => return Err(e),
            },
        };
        let uid = match (&self.uid, &project) {
            (Some(u), _) => u.clone(),
            (None, Some(p)) => p.uid.clone(),
            (None, None) => bail!("give --uid or run inside a project"),
        };
        let branch = match (&self.branch, &project) {
            (Some(b), _) => b.clone(),
            (None, Some(p)) => config::git_branch(&p.dir)?,
            (None, None) => "main".into(),
        };
        let server = config::server(self.server.as_deref(), project.as_ref());
        let api_key = config::api_key(project.as_ref());
        Ok(Target { project, uid, branch, client: Client::new(&server, api_key) })
    }
}

#[derive(Subcommand)]
enum Command {
    /// Start a project here (or in DIR): project.yml, datasources.yml, models/, tests/, samples/, security.yml.
    Init {
        dir: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        uid: Option<String>,
    },
    /// Generate a model file from a template.
    New {
        #[command(subcommand)]
        what: NewCommand,
    },
    /// Store the API key (and optionally a server) in .zsql for this project.
    Auth {
        #[arg(long)]
        api_key: String,
        #[arg(long)]
        server: Option<String>,
    },
    /// Deploy the checked-out branch: the model files, tests and samples (never secrets).
    Deploy {
        /// Validate only; the server keeps nothing.
        #[arg(long)]
        dry_run: bool,
        /// Redeploy whenever a project file changes.
        #[arg(long)]
        watch: bool,
    },
    /// Validate the project on the server without deploying (deploy --dry-run).
    Check,
    /// Run the deployed branch's tests/*.yml on the server.
    Test,
    /// Generate one query's SQL on the server.
    Sql {
        #[command(flatten)]
        query: QueryArgs,
        /// Explain the plan instead: node graph and timings, as `zsql explain`.
        #[arg(long)]
        explain: bool,
        /// Print the server's answer as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Explain how the server plans a query: SQL, node graph and timings.
    Explain {
        #[command(flatten)]
        query: QueryArgs,
        /// Print the server's answer as JSON.
        #[arg(long)]
        json: bool,
    },
    /// What a query can take on: the dimensions to drill into and the measures to add.
    Explore {
        #[command(flatten)]
        query: QueryArgs,
        /// Keep only fields whose name, uid or synonyms contain this, or come close.
        #[arg(long, short)]
        q: Option<String>,
        /// Print the server's answer as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the query spec a shorthand line makes, without calling the server.
    Spec {
        /// The line: projections, filters and calculations, comma-separated.
        #[arg(trailing_var_arg = true, required = true)]
        line: Vec<String>,
    },
    /// Plan interactively against the server.
    Repl {
        #[arg(long)]
        context: Option<PathBuf>,
    },
    /// Fields of the deployed branch, filtered by text.
    Fields {
        q: Option<String>,
        #[arg(long)]
        hidden: bool,
    },
    /// Tables of the deployed branch.
    Tables,
    /// Sampled values of a dimension.
    Values {
        #[command(subcommand)]
        what: ValuesCommand,
    },
    /// Summary of the deployed branch.
    Status,
    /// Every deployment on the server.
    List,
    /// Remove a deployed branch from the server.
    Remove {
        #[arg(long)]
        yes: bool,
    },
    /// Liveness of the server.
    Health,
}

#[derive(Subcommand)]
enum NewCommand {
    /// models/<domain>/tbl.<name>.yml
    Table {
        name: String,
        #[arg(long)]
        datasource: String,
        #[arg(long)]
        physical_name: Option<String>,
        #[arg(long, default_value = "core")]
        domain: String,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// models/<domain>/rel.<domain>.yml
    Relation {
        #[arg(long)]
        datasource: String,
        #[arg(long, default_value = "core")]
        domain: String,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// tests/<name>.yml
    Test {
        name: String,
        #[arg(long)]
        project: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ValuesCommand {
    /// Show a dimension's sampled values from the deployed branch.
    Show {
        dim: String,
    },
    /// Sample the listable dimensions from the project's warehouses into samples/values.jsonl.gz.
    /// The server says what to run (it knows the model); the SQL runs here, where the credentials are.
    Sample {
        /// Only these dimensions (uids or names).
        #[arg(long, value_delimiter = ',')]
        dims: Vec<String>,
        /// Every distinct value, not the top value_list_size (refused above the server's ceiling).
        #[arg(long)]
        full: bool,
        /// Resample only dimensions sampled more than this many days ago, or never.
        #[arg(long)]
        older_than: Option<i64>,
        /// Print the SQL that would run and stop.
        #[arg(long)]
        dry_run: bool,
        /// Deploy the project afterwards so the server has the new values.
        #[arg(long)]
        deploy: bool,
    },
}

fn main() -> Result<()> {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let Cli { command, at } = Cli::parse();
    let command = match command {
        Some(c) => c,
        None if at.project.is_some() || at.uid.is_some() || config::find_project(Path::new(".")).is_ok() => Command::Repl { context: None },
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!("\nInside a project, `zsql` alone opens the repl.");
            return Ok(());
        }
    };
    match command {
        Command::Spec { line } => {
            let (spec, items) = exprs::parse_line(&line.join(" ")).map_err(|e| anyhow::anyhow!(e))?;
            for d in exprs::describe(&items) {
                eprintln!("{d}");
            }
            println!("{}", serde_json::to_string_pretty(&spec)?);
            Ok(())
        }
        Command::Init { dir, name, uid } => init(dir, name, uid),
        Command::New { what } => new_file(what),
        Command::Auth { api_key, server } => {
            let project = match &at.project {
                Some(d) => config::read_project(d)?,
                None => config::find_project(Path::new("."))?,
            };
            config::write_secret(&project.dir, "api_key", &api_key)?;
            if let Some(s) = server {
                config::write_secret(&project.dir, "server", &s)?;
            }
            println!("saved to {}", project.dir.join(config::LOCAL_FILE).display());
            Ok(())
        }
        Command::Deploy { dry_run, watch } => deploy(at, dry_run, watch),
        Command::Check => deploy(at, true, false),
        Command::Test => {
            let t = at.resolve()?;
            let v = t.client.post(&format!("{}/test", remote::branch_path(&t.uid, &t.branch)), &json!({}))?;
            if !remote::print_tests(&v) {
                bail!("tests failed");
            }
            Ok(())
        }
        Command::Sql { query, explain, json } => query.run(at, explain, json),
        Command::Explain { query, json } => query.run(at, true, json),
        Command::Explore { query, q, json } => {
            let (t, spec_value, context) = query.resolve(at)?;
            let v = remote::explore(&t.client, &t.uid, &t.branch, spec_value, context.as_ref(), q.as_deref())?;
            if json {
                remote::print_json(&v)?;
            } else {
                remote::print_explore(&v, &|s| s.to_string());
            }
            Ok(())
        }
        Command::Repl { context } => repl(at.resolve()?, read_json(context)?),
        Command::Fields { q, hidden } => {
            let t = at.resolve()?;
            remote::print_fields(&t.client.get(&format!("{}/fields?q={}&hidden={hidden}", remote::branch_path(&t.uid, &t.branch), remote::urlencode(&q.unwrap_or_default())))?);
            Ok(())
        }
        Command::Tables => {
            let t = at.resolve()?;
            remote::print_tables(&t.client.get(&format!("{}/tables", remote::branch_path(&t.uid, &t.branch)))?);
            Ok(())
        }
        Command::Values { what } => values(at, what),
        Command::Status => {
            let t = at.resolve()?;
            remote::print_json(&t.client.get(&remote::branch_path(&t.uid, &t.branch))?)
        }
        Command::List => {
            let server = config::server(at.server.as_deref(), at.resolve().ok().and_then(|t| t.project).as_ref());
            let client = Client::new(&server, config::api_key(None));
            let v = client.get("/projects")?;
            for d in v["deployments"].as_array().into_iter().flatten() {
                println!("{:<32} {:<16} {} tables, {} fields, {} policies, {} values, {} tests", d["project"].as_str().unwrap_or(""), d["branch"].as_str().unwrap_or(""), d["tables"], d["fields"], d["policies"], d["values"], d["tests"]);
            }
            Ok(())
        }
        Command::Remove { yes } => {
            let t = at.resolve()?;
            if !yes {
                bail!("this removes {}/{} from {}; add --yes to confirm", t.uid, t.branch, t.client.server);
            }
            remote::print_json(&t.client.delete(&remote::branch_path(&t.uid, &t.branch))?)
        }
        Command::Health => {
            let server = config::server(at.server.as_deref(), config::find_project(Path::new(".")).ok().as_ref());
            let v = Client::new(&server, None).get("/healthz")?;
            println!("{} {}", server, v.as_str().unwrap_or("ok"));
            Ok(())
        }
    }
}

/// What `zsql sql` and `zsql explain` take: the query, as a spec file or a
/// shorthand line, and the security context.
#[derive(clap::Args)]
struct QueryArgs {
    /// A JSON file holding a query spec.
    #[arg(long, conflicts_with = "expr")]
    spec: Option<PathBuf>,
    /// A comma-separated line of projections, filters and calculations.
    #[arg(long)]
    expr: Option<String>,
    /// A JSON file holding the security context.
    #[arg(long)]
    context: Option<PathBuf>,
}

impl QueryArgs {
    /// The target, the spec and the context the arguments name.
    fn resolve(self, at: Where) -> Result<(Target, Value, Option<Value>)> {
        let t = at.resolve()?;
        let spec_value = match (self.spec, self.expr) {
            (Some(path), _) => {
                let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
                let value: Value = serde_json::from_str(&text)?;
                value.get("spec").cloned().unwrap_or(value)
            }
            (None, Some(line)) => {
                let (spec, items) = exprs::parse_line(&line).map_err(|e| anyhow::anyhow!(e))?;
                for d in exprs::describe(&items) {
                    eprintln!("{d}");
                }
                spec
            }
            (None, None) => bail!("give the query as --spec FILE or --expr LINE"),
        };
        let context = read_json(self.context)?;
        Ok((t, spec_value, context))
    }

    fn run(self, at: Where, explain: bool, json: bool) -> Result<()> {
        let (t, spec_value, context) = self.resolve(at)?;
        let v = remote::sql(&t.client, &t.uid, &t.branch, spec_value, context.as_ref(), explain)?;
        if json {
            remote::print_json(&v)?;
        } else if explain {
            remote::print_explain(&v, &|s| s.to_string());
        } else {
            for c in remote::corrections(&v) {
                eprintln!("{c}");
            }
            println!("{}", remote::datasource_line(&v));
            println!("{}", v["sql"].as_str().unwrap_or(""));
        }
        Ok(())
    }
}

fn read_json(path: Option<PathBuf>) -> Result<Option<Value>> {
    Ok(match path {
        Some(p) => Some(serde_json::from_str(&std::fs::read_to_string(&p).with_context(|| p.display().to_string())?)?),
        None => None,
    })
}

// ------------------------------------------------------------------- init

fn init(dir: Option<PathBuf>, name: Option<String>, uid: Option<String>) -> Result<()> {
    let dir = dir.unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    if dir.join("project.yml").exists() {
        bail!("{} already holds a project", dir.display());
    }
    let name = name.unwrap_or_else(|| dir.canonicalize().ok().and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string())).unwrap_or_else(|| "project".into()));
    let uid = uid.map(|u| u.to_lowercase()).unwrap_or_else(|| config::slug(&name));
    let write = |rel: &str, text: &str| -> Result<()> {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if path.exists() {
            println!("keep   {rel}");
            return Ok(());
        }
        std::fs::write(&path, text)?;
        println!("create {rel}");
        Ok(())
    };
    write("project.yml", &templates::project_yml(&name, &uid))?;
    write("datasources.yml", templates::DATASOURCES_YML)?;
    write("security.yml", templates::SECURITY_YML)?;
    write("models/.keep", "")?;
    write("tests/.keep", "")?;
    write("samples/README.md", templates::SAMPLES_README)?;
    let gi = dir.join(".gitignore");
    let existing = std::fs::read_to_string(&gi).unwrap_or_default();
    if !existing.contains(".zsql") {
        std::fs::write(&gi, format!("{existing}{}{}", if existing.is_empty() || existing.ends_with('\n') { "" } else { "\n" }, templates::GITIGNORE))?;
        println!("update .gitignore");
    }
    if !dir.join(".git").exists() {
        let ok = std::process::Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(&dir).status().map(|s| s.success()).unwrap_or(false);
        if ok {
            println!("git init (branch main)");
        }
    }
    println!("\nnext: describe your warehouses in datasources.yml, `zsql new table ...`, `zsql auth --api-key ...`, `zsql deploy`");
    Ok(())
}

fn new_file(what: NewCommand) -> Result<()> {
    let (project_dir, rel, text) = match what {
        NewCommand::Table { name, datasource, physical_name, domain, project } => {
            let p = project_of(project)?;
            let file = config::slug(&name).replace('-', "_");
            (p.dir, format!("models/{domain}/tbl.{file}.yml"), templates::table_yml(&name, physical_name.as_deref().unwrap_or(&file), &datasource))
        }
        NewCommand::Relation { datasource, domain, project } => {
            let p = project_of(project)?;
            (p.dir, format!("models/{domain}/rel.{domain}.yml"), templates::relation_yml(&datasource))
        }
        NewCommand::Test { name, project } => {
            let p = project_of(project)?;
            let file = config::slug(&name).replace('-', "_");
            (p.dir, format!("tests/{file}.yml"), templates::test_yml(&name))
        }
    };
    let path = project_dir.join(&rel);
    if path.exists() {
        bail!("{} exists", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, text)?;
    println!("create {rel}");
    Ok(())
}

fn project_of(dir: Option<PathBuf>) -> Result<Project> {
    match dir {
        Some(d) => config::read_project(&d),
        None => config::find_project(Path::new(".")),
    }
}

// ----------------------------------------------------------------- deploy

fn deploy(at: Where, dry_run: bool, watch: bool) -> Result<()> {
    let t = at.resolve()?;
    let Some(project) = &t.project else { bail!("deploy needs a project directory") };
    let action = if dry_run { "validate" } else { "deploy" };
    let once = |t: &Target| -> Result<bool> {
        let archive = project_archive(&project.dir)?;
        let note = if t.branch == project.production_branch { " (the production branch)" } else { "" };
        println!("{action} {} ({} bytes) as {}/{}{note} to {}", project.name, archive.len(), t.uid, t.branch, t.client.server);
        let v = t.client.post_bytes(&format!("{}/{action}", remote::branch_path(&t.uid, &t.branch)), "application/gzip", &archive)?;
        Ok(remote::print_deploy(&v))
    };
    if !watch {
        if !once(&t)? {
            bail!("tests failed");
        }
        return Ok(());
    }
    let _ = once(&t);
    println!("watching {} for changes (ctrl-c to stop)", project.dir.display());
    let mut last = newest_mtime(&project.dir);
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let now = newest_mtime(&project.dir);
        if now != last {
            std::thread::sleep(Duration::from_millis(200));
            println!();
            if let Err(e) = once(&t) {
                println!("error: {e}");
            }
            last = newest_mtime(&project.dir);
        }
    }
}

fn newest_mtime(dir: &Path) -> Option<std::time::SystemTime> {
    let mut newest = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || name == "target" {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else if matches!(p.extension().and_then(|x| x.to_str()), Some("yml") | Some("yaml") | Some("gz")) {
                if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                    newest = Some(newest.map_or(m, |n: std::time::SystemTime| n.max(m)));
                }
            }
        }
    }
    newest
}

/// The project as a tar.gz: YAML models, tests, security, datasources.yml
/// with its secrets removed, and samples/values.jsonl.gz. Never .zsql,
/// .strata, .git or data files.
fn project_archive(project: &Path) -> Result<Vec<u8>> {
    const SECRET_KEYS: [&str; 8] = ["password", "private_key", "personal_access_token", "secret_access_key", "access_key_id", "oauth_client_secret", "oauth_client_id", "api_key"];
    let mut buf = Vec::new();
    {
        let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        let mut add = |rel: &str, bytes: &[u8]| -> Result<()> {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, rel, bytes)?;
            Ok(())
        };
        for name in ["project.yml", "security.yml"] {
            if let Ok(bytes) = std::fs::read(project.join(name)) {
                add(name, &bytes)?;
            }
        }
        if let Ok(text) = std::fs::read_to_string(project.join("datasources.yml")) {
            let mut v: serde_yaml::Value = serde_yaml::from_str(&text)?;
            if let Some(map) = v.as_mapping_mut() {
                for (_, ds) in map.iter_mut() {
                    if let Some(entry) = ds.as_mapping_mut() {
                        for k in SECRET_KEYS {
                            entry.remove(serde_yaml::Value::String(k.into()));
                        }
                    }
                }
            }
            add("datasources.yml", serde_yaml::to_string(&v)?.as_bytes())?;
        }
        if let Ok(bytes) = std::fs::read(project.join("samples").join("values.jsonl.gz")) {
            add("samples/values.jsonl.gz", &bytes)?;
        }
        for dir in ["models", "tests"] {
            let root = project.join(dir);
            if !root.is_dir() {
                continue;
            }
            let mut files = Vec::new();
            walk_yaml(&root, &mut files)?;
            files.sort();
            for f in files {
                let rel = f.strip_prefix(project)?.to_string_lossy().replace('\\', "/");
                add(&rel, &std::fs::read(&f)?)?;
            }
        }
        tar.into_inner()?.finish()?;
    }
    Ok(buf)
}

fn walk_yaml(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            walk_yaml(&path, out)?;
        } else if matches!(path.extension().and_then(|e| e.to_str()), Some("yml") | Some("yaml")) {
            out.push(path);
        }
    }
    Ok(())
}

// ----------------------------------------------------------------- values

fn values(at: Where, what: ValuesCommand) -> Result<()> {
    match what {
        ValuesCommand::Show { dim } => {
            let t = at.resolve()?;
            remote::print_values(&remote::values_for(&t.client, &t.uid, &t.branch, &dim)?);
            Ok(())
        }
        ValuesCommand::Sample { dims, full, older_than, dry_run, deploy: deploy_after } => {
            let t = at.resolve()?;
            let Some(project) = t.project.clone() else { bail!("sampling needs a project directory (the credentials live there)") };
            let plan = t.client.get(&format!("{}/sampling-plan?dims={}&full={full}", remote::branch_path(&t.uid, &t.branch), remote::urlencode(&dims.join(","))))?;
            let jobs = plan["jobs"].as_array().cloned().unwrap_or_default();
            let skipped = plan["skipped"].as_array().cloned().unwrap_or_default();
            if !dims.is_empty() {
                for s in &skipped {
                    println!("skip {}: {}", s["uid"].as_str().unwrap_or(""), s["reason"].as_str().unwrap_or(""));
                }
            }
            let now = chrono::Utc::now();
            let cut = older_than.map(|days| now - chrono::Duration::days(days));
            let todo: Vec<&Value> = jobs
                .iter()
                .filter(|j| match (cut, j["sampled_at"].as_str()) {
                    (Some(cut), Some(at)) => chrono::DateTime::parse_from_rfc3339(at).map(|t| t < cut).unwrap_or(true),
                    _ => true,
                })
                .collect();
            if dry_run {
                for j in &todo {
                    println!("-- {} ({}) on {} via {}\n{}\n{}\n", j["uid"].as_str().unwrap_or(""), j["name"].as_str().unwrap_or(""), j["table"].as_str().unwrap_or(""), j["datasource"].as_str().unwrap_or(""), j["distinct_sql"].as_str().unwrap_or(""), j["values_sql"].as_str().or(j["range_sql"].as_str()).unwrap_or(""));
                }
                println!("{} dimensions to sample, {} skipped", todo.len(), skipped.len());
                return Ok(());
            }
            let samples_dir = project.dir.join("samples");
            std::fs::create_dir_all(&samples_dir)?;
            let path = samples_dir.join("values.jsonl.gz");
            let mut catalog = read_catalog(&path)?;
            let ceiling = plan["full_ceiling"].as_i64().unwrap_or(100_000);
            let mut connections: std::collections::HashMap<String, Box<dyn zsql_warehouse::Warehouse>> = Default::default();
            let (mut done, mut failed) = (0, 0);
            for j in &todo {
                let ds = j["datasource"].as_str().unwrap_or("").to_string();
                if !connections.contains_key(&ds) {
                    let settings = zsql_warehouse::settings_for(&project.dir, &ds)?;
                    match zsql_warehouse::connect(&settings) {
                        Ok(c) => {
                            connections.insert(ds.clone(), c);
                        }
                        Err(e) => bail!("datasource {ds}: {e}"),
                    }
                }
                let wh = connections.get_mut(&ds).unwrap();
                let uid = j["uid"].as_str().unwrap_or("").to_string();
                let outcome = (|| -> Result<Value> {
                    let distinct = wh.query(j["distinct_sql"].as_str().unwrap_or(""))?.first().and_then(|r| r.first().cloned()).flatten().and_then(|n| n.parse::<i64>().ok());
                    let mut entry = json!({
                        "uid": uid, "name": j["name"], "table": j["table"],
                        "sampled_at": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        "mode": j["mode"], "distinct": distinct,
                    });
                    if let Some(sql) = j["values_sql"].as_str() {
                        if j["cap"].is_null() && distinct.map(|n| n > ceiling).unwrap_or(false) {
                            bail!("{uid} has {} distinct values; a full sample is refused above {ceiling}", distinct.unwrap());
                        }
                        let values: Vec<Value> = wh
                            .query(sql)?
                            .into_iter()
                            .filter_map(|r| Some(json!({ "v": r.first().cloned().flatten()?, "n": r.get(1).cloned().flatten().and_then(|n| n.parse::<i64>().ok()).unwrap_or(0) })))
                            .collect();
                        entry["values"] = Value::Array(values);
                    }
                    if let Some(sql) = j["range_sql"].as_str() {
                        let row = wh.query(sql)?.into_iter().next().unwrap_or_default();
                        if let (Some(Some(lo)), Some(Some(hi))) = (row.first().cloned(), row.get(1).cloned()) {
                            entry["range"] = json!({ "min": lo, "max": hi });
                        }
                    }
                    Ok(entry)
                })();
                match outcome {
                    Ok(entry) => {
                        let what = match (entry.get("range"), entry["values"].as_array().map(Vec::len)) {
                            (Some(r), _) => format!("{} .. {}", r["min"].as_str().unwrap_or(""), r["max"].as_str().unwrap_or("")),
                            (None, n) => format!("{} values", n.unwrap_or(0)),
                        };
                        println!("{:<40} {what}", uid);
                        catalog.retain(|e| e["uid"] != uid);
                        catalog.push(entry);
                        done += 1;
                    }
                    Err(e) => {
                        failed += 1;
                        eprintln!("{:<40} FAILED: {e}", uid);
                    }
                }
            }
            catalog.sort_by(|a, b| a["uid"].as_str().cmp(&b["uid"].as_str()));
            write_catalog(&path, &catalog)?;
            println!("{done} sampled, {failed} failed, {} skipped; wrote {}", skipped.len(), path.display());
            if failed > 0 {
                bail!("{failed} dimension(s) failed to sample");
            }
            if deploy_after {
                deploy(at, false, false)?;
            }
            Ok(())
        }
    }
}

fn read_catalog(path: &Path) -> Result<Vec<Value>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(flate2::read::GzDecoder::new(file));
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if !line.trim().is_empty() {
            out.push(serde_json::from_str(&line)?);
        }
    }
    Ok(out)
}

fn write_catalog(path: &Path, entries: &[Value]) -> Result<()> {
    let file = std::fs::File::create(path)?;
    let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    for e in entries {
        enc.write_all(serde_json::to_string(e)?.as_bytes())?;
        enc.write_all(b"\n")?;
    }
    enc.finish()?;
    Ok(())
}

// ------------------------------------------------------------------- repl

fn repl(mut t: Target, context: Option<Value>) -> Result<()> {
    let tty = std::io::stdout().is_terminal();
    let dim = |s: &str| if tty { format!("\x1b[2m{s}\x1b[0m") } else { s.to_string() };
    let bold = |s: &str| if tty { format!("\x1b[1m{s}\x1b[0m") } else { s.to_string() };
    let mut field_names: Vec<String> = Vec::new();
    let announce = |t: &Target, field_names: &mut Vec<String>| -> Result<()> {
        let summary = t.client.get(&remote::branch_path(&t.uid, &t.branch))?;
        println!("{}", dim(&format!("{}/{} on {}: {} tables, {} fields, {} joins, {} policies", t.uid, t.branch, t.client.server, summary["tables"], summary["fields"], summary["joins"], summary["policies"])));
        *field_names = t
            .client
            .get(&format!("{}/fields?hidden=true", remote::branch_path(&t.uid, &t.branch)))?["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|f| [f["uid"].as_str().unwrap_or("").to_lowercase(), f["name"].as_str().unwrap_or("").to_lowercase()])
            .collect();
        Ok(())
    };
    announce(&t, &mut field_names)?;
    println!("{}", dim("type a line of projections and filters; .help for the forms, .quit to leave"));

    let mut editor: Option<rustyline::DefaultEditor> = if std::io::stdin().is_terminal() { rustyline::DefaultEditor::new().ok() } else { None };
    let history = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".zsql").join("history"));
    if let (Some(ed), Some(h)) = (editor.as_mut(), &history) {
        let _ = std::fs::create_dir_all(h.parent().unwrap());
        let _ = ed.load_history(h);
    }
    let stdin = std::io::stdin();
    loop {
        let line = match editor.as_mut() {
            Some(ed) => match ed.readline("zsql> ") {
                Ok(l) => {
                    let _ = ed.add_history_entry(l.as_str());
                    l
                }
                Err(rustyline::error::ReadlineError::Interrupted) => continue,
                Err(_) => break,
            },
            None => {
                print!("zsql> ");
                std::io::stdout().flush()?;
                let mut l = String::new();
                if stdin.lock().read_line(&mut l)? == 0 {
                    break;
                }
                l
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (word, rest) = line.split_once(' ').map(|(c, r)| (c, r.trim())).unwrap_or((line, ""));
        let names_a_field = |w: &str| field_names.iter().any(|n| n == &w.to_lowercase());
        let cmd = match word.strip_prefix('.') {
            Some(c) => c,
            None if matches!(word, "help" | "?" | "quit" | "exit" | "q") && rest.is_empty() && !names_a_field(word) => word,
            None => "",
        };
        if matches!(cmd, "quit" | "exit" | "q") {
            break;
        }
        let outcome: Result<()> = (|| {
            match cmd {
                "help" | "?" => println!("{}", exprs::HELP),
                "tables" => remote::print_tables(&t.client.get(&format!("{}/tables", remote::branch_path(&t.uid, &t.branch)))?),
                "fields" => remote::print_fields(&t.client.get(&format!("{}/fields?q={}", remote::branch_path(&t.uid, &t.branch), remote::urlencode(rest)))?),
                "values" => {
                    if rest.is_empty() {
                        println!("usage: .values <dimension>");
                    } else {
                        remote::print_values(&remote::values_for(&t.client, &t.uid, &t.branch, rest)?);
                    }
                }
                "branch" => {
                    if rest.is_empty() {
                        println!("{}", t.branch);
                    } else {
                        t.branch = rest.to_string();
                        announce(&t, &mut field_names)?;
                    }
                }
                "spec" => match exprs::parse_line(rest) {
                    Ok((spec, items)) => {
                        for d in exprs::describe(&items) {
                            println!("{}", dim(&d));
                        }
                        println!("{}", serde_json::to_string_pretty(&spec)?);
                    }
                    Err(e) => println!("error: {e}"),
                },
                "" | "json" | "explain" | "explore" => {
                    // A query: the shorthand, or a JSON spec after .json or
                    // starting with a brace. .explain and .explore take
                    // either; .explore narrows by a term after ` ? `.
                    let text = if cmd == "" { line } else { rest };
                    let (text, term) = match cmd {
                        "explore" => text.rsplit_once(" ? ").map(|(l, q)| (l.trim(), Some(q.trim()))).unwrap_or((text, None)),
                        _ => (text, None),
                    };
                    let spec: Result<Value, String> = if cmd == "json" || text.starts_with('{') {
                        serde_json::from_str(text).map_err(|e| e.to_string())
                    } else {
                        exprs::parse_line(text).map(|(spec, items)| {
                            for d in exprs::describe(&items) {
                                println!("{}", dim(&d));
                            }
                            spec
                        })
                    };
                    match spec {
                        Err(e) => println!("error: {e}"),
                        Ok(spec) if cmd == "explore" => match remote::explore(&t.client, &t.uid, &t.branch, spec, context.as_ref(), term) {
                            Ok(v) => remote::print_explore(&v, &dim),
                            Err(e) => println!("error: {e}"),
                        },
                        Ok(spec) => {
                            let t0 = Instant::now();
                            match remote::sql(&t.client, &t.uid, &t.branch, spec, context.as_ref(), cmd == "explain") {
                                Ok(v) if cmd == "explain" => remote::print_explain(&v, &dim),
                                Ok(v) => {
                                    for c in remote::corrections(&v) {
                                        println!("{}", dim(&c));
                                    }
                                    println!("{}", dim(&remote::datasource_line(&v)));
                                    println!("{}", bold(v["sql"].as_str().unwrap_or("")));
                                    println!("{}", dim(&format!("-- round trip {:.2} ms", t0.elapsed().as_secs_f64() * 1e3)));
                                }
                                Err(e) => println!("error: {e}"),
                            }
                        }
                    }
                }
                other => println!("unknown command .{other}; .help lists them"),
            }
            Ok(())
        })();
        if let Err(e) = outcome {
            println!("error: {e}");
        }
    }
    if let (Some(ed), Some(h)) = (editor.as_mut(), &history) {
        let _ = ed.save_history(h);
    }
    Ok(())
}

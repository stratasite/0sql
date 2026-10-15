//! zsql: author a semantic model, plan SQL from it, deploy it.
//!
//! One rule decides where a command runs. Anything that needs only the
//! model runs here, from the project directory at hand: sql, explain,
//! explore, fields, tables, values, check, test. Anything about a
//! server's copy of the model needs `-r`: deploy, status, list, remove,
//! and the same reads against what the server holds. `--server` and
//! `--uid` imply `-r`; `ZSQL_SERVER` in the environment does not.

mod config;
mod failure;
mod local;
mod remote;
mod repl;
mod show;
mod templates;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use config::Project;
use local::Local;
use remote::Client;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
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

/// Which project a command works on, and whether here or on a server.
#[derive(Args, Clone, Default)]
struct Where {
    /// Project directory; defaults to the one holding project.yml here or above.
    #[arg(long, global = true)]
    project: Option<PathBuf>,
    /// Use the server's copy of the model rather than the files here.
    #[arg(short = 'r', long, global = true)]
    remote: bool,
    /// Branch on the server; defaults to the checked-out git branch. Implies nothing here: the files are what they are.
    #[arg(long, global = true)]
    branch: Option<String>,
    /// The zsqld server (implies -r); defaults to ZSQL_SERVER, .zsql, project.yml, then the local server.
    #[arg(long, global = true)]
    server: Option<String>,
    /// Project uid on the server (implies -r), when there is no project directory at hand.
    #[arg(long, global = true)]
    uid: Option<String>,
}

/// A server's copy of a project's branch.
struct Target {
    project: Option<Project>,
    uid: String,
    branch: String,
    client: Client,
}

/// Where a command runs.
enum Mode {
    Local(Local),
    Remote(Target),
}

impl Where {
    fn wants_remote(&self) -> bool {
        self.remote || self.server.is_some() || self.uid.is_some()
    }

    /// The project directory at hand, unless the flags ask for a server.
    fn mode(&self) -> Result<Mode> {
        if self.wants_remote() {
            return Ok(Mode::Remote(self.target()?));
        }
        if let Some(b) = &self.branch {
            bail!("--branch {b} names a branch on the server; add -r to plan against it, or check it out to plan it here");
        }
        Ok(Mode::Local(Local { project: self.project_dir()? }))
    }

    fn project_dir(&self) -> Result<Project> {
        match &self.project {
            Some(dir) => config::read_project(dir),
            None => config::find_project(Path::new(".")),
        }
    }

    /// The server target, for commands that are about a server's copy.
    /// This is the one place the git branch is read.
    fn target(&self) -> Result<Target> {
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
    /// Load and validate the project here, and run its tests; with -r, validate on the server.
    Check,
    /// Run tests/*.yml here; with -r, the deployed branch's tests on the server.
    Test,
    /// Generate one query's SQL.
    Sql {
        #[command(flatten)]
        query: QueryArgs,
        /// Explain the plan instead: node graph and timings, as `zsql explain`.
        #[arg(long)]
        explain: bool,
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Explain how a query is planned: SQL, node graph and timings.
    Explain {
        #[command(flatten)]
        query: QueryArgs,
        /// Print the answer as JSON.
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
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the query spec a shorthand line makes.
    Spec {
        /// The line: projections, filters and calculations, comma-separated.
        #[arg(trailing_var_arg = true, required = true)]
        line: Vec<String>,
    },
    /// Plan interactively: the model here, reloaded as its files change; with -r, the server's copy.
    Repl {
        #[arg(long)]
        context: Option<PathBuf>,
    },
    /// Fields of the model, filtered by text.
    Fields {
        q: Option<String>,
        #[arg(long)]
        hidden: bool,
    },
    /// Tables of the model.
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

impl Command {
    /// Whether the command answers in JSON, which is where a refusal
    /// goes too.
    fn wants_json(&self) -> bool {
        matches!(self, Command::Sql { json: true, .. } | Command::Explain { json: true, .. } | Command::Explore { json: true, .. })
    }
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
    /// Show a dimension's sampled values.
    Show { dim: String },
    /// Sample the listable dimensions from the project's warehouses into samples/values.jsonl.gz.
    /// The model says what to run; the SQL runs here, where the credentials are.
    Sample {
        /// Only these dimensions (uids or names).
        #[arg(long, value_delimiter = ',')]
        dims: Vec<String>,
        /// Every distinct value, not the top value_list_size (refused above the ceiling).
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

fn main() {
    let Cli { command, at } = Cli::parse();
    let command = match command {
        Some(c) => c,
        None if at.project.is_some() || at.uid.is_some() || config::find_project(Path::new(".")).is_ok() => Command::Repl { context: None },
        None => {
            use clap::CommandFactory;
            let _ = Cli::command().print_help();
            println!("\nInside a project, `zsql` alone opens the repl.");
            return;
        }
    };
    // A closed pipe (`zsql ... | head`) ends the command quietly instead
    // of panicking. A command that runs a server must not do this, since
    // a client hanging up mid-answer would then kill it; when one
    // exists, it decides here before the reset.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let json = command.wants_json();
    if let Err(e) = run(command, at) {
        failure::report(&e, json);
        std::process::exit(1);
    }
}

fn run(command: Command, at: Where) -> Result<()> {
    match command {
        Command::Spec { line } => {
            let (spec, _, items) =
                zsql_service::spec_of(None, Some(&line.join(" ")), &zsql_service::unlimited).map_err(failure::Refusal::from)?;
            for d in items {
                eprintln!("{d}");
            }
            println!("{}", serde_json::to_string_pretty(&spec)?);
            Ok(())
        }
        Command::Init { dir, name, uid } => init(dir, name, uid),
        Command::New { what } => new_file(what),
        Command::Auth { api_key, server } => {
            let project = at.project_dir()?;
            config::write_secret(&project.dir, "api_key", &api_key)?;
            if let Some(s) = server {
                config::write_secret(&project.dir, "server", &s)?;
            }
            println!("saved to {}", project.dir.join(config::LOCAL_FILE).display());
            Ok(())
        }
        Command::Deploy { dry_run, watch } => deploy(at, dry_run, watch),
        Command::Check => match at.mode()? {
            Mode::Local(l) => {
                let t0 = Instant::now();
                let m = l.model()?;
                let v = local::check(&m);
                let ok = show::print_deploy(&v);
                println!("-- local {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
                if !ok {
                    bail!("tests failed");
                }
                Ok(())
            }
            Mode::Remote(_) => deploy(at, true, false),
        },
        Command::Test => {
            let results = match at.mode()? {
                Mode::Local(l) => l.model()?.run_tests(),
                Mode::Remote(t) => t.client.post(&format!("{}/test", remote::branch_path(&t.uid, &t.branch)), &json!({}))?,
            };
            if !show::print_tests(&results) {
                bail!("tests failed");
            }
            Ok(())
        }
        Command::Sql { query, explain, json } => query.run(at, explain, json),
        Command::Explain { query, json } => query.run(at, true, json),
        Command::Explore { query, q, json } => {
            let (mode, Query { spec, expr, context }) = query.resolve(at)?;
            let (v, origin) = match &mode {
                Mode::Local(l) => (local::explore(&l.model()?, spec, expr.as_deref(), context.as_ref(), q.as_deref())?, "local"),
                Mode::Remote(t) => (
                    remote::explore(&t.client, &t.uid, &t.branch, spec.as_ref(), expr.as_deref(), context.as_ref(), q.as_deref())?,
                    "server",
                ),
            };
            if json {
                show::print_json(&v)?;
            } else {
                show::print_explore(&v, &|s| s.to_string(), origin);
            }
            Ok(())
        }
        Command::Repl { context } => {
            let context = read_json(context)?;
            match at.mode()? {
                Mode::Local(l) => repl::run(repl::Session::open(l)?, context),
                Mode::Remote(t) => repl::run(t, context),
            }
        }
        Command::Fields { q, hidden } => {
            let q = q.unwrap_or_default();
            let v = match at.mode()? {
                Mode::Local(l) => l.model()?.fields(&q, hidden),
                Mode::Remote(t) => t.client.get(&format!(
                    "{}/fields?q={}&hidden={hidden}",
                    remote::branch_path(&t.uid, &t.branch),
                    remote::urlencode(&q)
                ))?,
            };
            show::print_fields(&v);
            Ok(())
        }
        Command::Tables => {
            let v = match at.mode()? {
                Mode::Local(l) => l.model()?.tables(),
                Mode::Remote(t) => t.client.get(&format!("{}/tables", remote::branch_path(&t.uid, &t.branch)))?,
            };
            show::print_tables(&v);
            Ok(())
        }
        Command::Values { what } => values(at, what),
        Command::Status => {
            let t = at.target()?;
            show::print_json(&t.client.get(&remote::branch_path(&t.uid, &t.branch))?)
        }
        Command::List => {
            let server = config::server(at.server.as_deref(), at.target().ok().and_then(|t| t.project).as_ref());
            let client = Client::new(&server, config::api_key(None));
            let v = client.get("/projects")?;
            for d in v["deployments"].as_array().into_iter().flatten() {
                println!(
                    "{:<32} {:<16} {} tables, {} fields, {} policies, {} values, {} tests",
                    d["project"].as_str().unwrap_or(""),
                    d["branch"].as_str().unwrap_or(""),
                    d["tables"],
                    d["fields"],
                    d["policies"],
                    d["values"],
                    d["tests"]
                );
            }
            Ok(())
        }
        Command::Remove { yes } => {
            let t = at.target()?;
            if !yes {
                bail!("this removes {}/{} from {}; add --yes to confirm", t.uid, t.branch, t.client.server);
            }
            show::print_json(&t.client.delete(&remote::branch_path(&t.uid, &t.branch))?)
        }
        Command::Health => {
            let server = config::server(at.server.as_deref(), config::find_project(Path::new(".")).ok().as_ref());
            let v = Client::new(&server, None).get("/healthz")?;
            println!("{} {}", server, v.as_str().unwrap_or("ok"));
            Ok(())
        }
    }
}

/// What `zsql sql`, `zsql explain` and `zsql explore` take: the query, as
/// a spec file or a shorthand line, and the security context.
#[derive(clap::Args)]
struct QueryArgs {
    /// A JSON file holding a query spec.
    #[arg(long, conflicts_with = "expr", required_unless_present = "expr")]
    spec: Option<PathBuf>,
    /// A comma-separated line of projections, filters and calculations.
    #[arg(long, required_unless_present = "spec")]
    expr: Option<String>,
    /// A JSON file holding the security context.
    #[arg(long)]
    context: Option<PathBuf>,
}

/// A query as it travels: the spec, or the shorthand line for whoever
/// plans it to read, and the context.
struct Query {
    spec: Option<Value>,
    expr: Option<String>,
    context: Option<Value>,
}

impl QueryArgs {
    /// Where to plan, and the query. The line is read by whoever plans
    /// it, here or on the server, so the item descriptions come from
    /// one place.
    fn resolve(self, at: Where) -> Result<(Mode, Query)> {
        let mode = at.mode()?;
        let (spec, expr) = match (self.spec, self.expr) {
            (Some(path), _) => {
                let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
                let value: Value = serde_json::from_str(&text)?;
                (Some(value.get("spec").cloned().unwrap_or(value)), None)
            }
            (None, Some(line)) => (None, Some(line)),
            // clap requires one of the two; this arm is for the compiler.
            (None, None) => bail!("give the query as --spec FILE or --expr LINE"),
        };
        let context = read_json(self.context)?;
        Ok((mode, Query { spec, expr, context }))
    }

    fn run(self, at: Where, explain: bool, json: bool) -> Result<()> {
        let (mode, Query { spec, expr, context }) = self.resolve(at)?;
        let t0 = Instant::now();
        let (v, origin) = match &mode {
            Mode::Local(l) => (local::sql(&l.model()?, spec, expr.as_deref(), context.as_ref(), explain)?, "local"),
            Mode::Remote(t) => {
                (remote::sql(&t.client, &t.uid, &t.branch, spec.as_ref(), expr.as_deref(), context.as_ref(), explain)?, "server")
            }
        };
        if json {
            show::print_json(&v)?;
        } else if explain {
            show::print_explain(&v, &|s| s.to_string(), origin);
        } else {
            for i in show::items(&v) {
                eprintln!("{i}");
            }
            for c in show::corrections(&v) {
                eprintln!("{c}");
            }
            println!("{}", show::datasource_line(&v));
            println!("{}", v["sql"].as_str().unwrap_or(""));
            if matches!(mode, Mode::Local(_)) {
                eprintln!("-- planned here in {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
            }
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
    let name = name.unwrap_or_else(|| {
        dir.canonicalize().ok().and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string())).unwrap_or_else(|| "project".into())
    });
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
        std::fs::write(
            &gi,
            format!("{existing}{}{}", if existing.is_empty() || existing.ends_with('\n') { "" } else { "\n" }, templates::GITIGNORE),
        )?;
        println!("update .gitignore");
    }
    if !dir.join(".git").exists() {
        let ok = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            // Deploying reads the checked-out branch, so an uncommitted
            // project cannot be deployed. Say so here, where it is a step on
            // a list, rather than at the deploy that fails.
            println!("git init (branch main) — commit before deploying; zsql deploys the checked-out branch");
        }
    }
    println!("\nnext: describe your warehouses in datasources.yml, `zsql new table ...`, `zsql sql --expr ...`; then `git commit`, `zsql auth --api-key ...`, `zsql deploy`");
    Ok(())
}

fn new_file(what: NewCommand) -> Result<()> {
    let (project_dir, rel, text) = match what {
        NewCommand::Table { name, datasource, physical_name, domain, project } => {
            let p = project_of(project)?;
            let file = config::slug(&name).replace('-', "_");
            (
                p.dir,
                format!("models/{domain}/tbl.{file}.yml"),
                templates::table_yml(&name, physical_name.as_deref().unwrap_or(&file), &datasource),
            )
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
    let t = at.target()?;
    let Some(project) = &t.project else { bail!("deploy needs a project directory") };
    let action = if dry_run { "validate" } else { "deploy" };
    let once = |t: &Target| -> Result<bool> {
        let archive = project_archive(&project.dir)?;
        let note = if t.branch == project.production_branch { " (the production branch)" } else { "" };
        println!("{action} {} ({} bytes) as {}/{}{note} to {}", project.name, archive.len(), t.uid, t.branch, t.client.server);
        let v = t.client.post_bytes(&format!("{}/{action}", remote::branch_path(&t.uid, &t.branch)), "application/gzip", &archive)?;
        Ok(show::print_deploy(&v))
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
    const SECRET_KEYS: [&str; 8] = [
        "password",
        "private_key",
        "personal_access_token",
        "secret_access_key",
        "access_key_id",
        "oauth_client_secret",
        "oauth_client_id",
        "api_key",
    ];
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
            let v = match at.mode()? {
                Mode::Local(l) => local::values_for(&l.model()?, &dim)?,
                Mode::Remote(t) => remote::values_for(&t.client, &t.uid, &t.branch, &dim)?,
            };
            show::print_values(&v);
            Ok(())
        }
        ValuesCommand::Sample { dims, full, older_than, dry_run, deploy: deploy_after } => {
            // The plan comes from the model, here or on the server; the
            // SQL runs here either way, where the credentials are.
            let (project, plan) = match at.mode()? {
                Mode::Local(l) => {
                    let plan = l.model()?.sampling_plan(&dims, full);
                    (l.project, plan)
                }
                Mode::Remote(t) => {
                    let Some(project) = t.project.clone() else { bail!("sampling needs a project directory (the credentials live there)") };
                    let plan = t.client.get(&format!(
                        "{}/sampling-plan?dims={}&full={full}",
                        remote::branch_path(&t.uid, &t.branch),
                        remote::urlencode(&dims.join(","))
                    ))?;
                    (project, plan)
                }
            };
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
                    println!(
                        "-- {} ({}) on {} via {}\n{}\n{}\n",
                        j["uid"].as_str().unwrap_or(""),
                        j["name"].as_str().unwrap_or(""),
                        j["table"].as_str().unwrap_or(""),
                        j["datasource"].as_str().unwrap_or(""),
                        j["distinct_sql"].as_str().unwrap_or(""),
                        j["values_sql"].as_str().or(j["range_sql"].as_str()).unwrap_or("")
                    );
                }
                println!("{} dimensions to sample, {} skipped", todo.len(), skipped.len());
                return Ok(());
            }
            let samples_dir = project.dir.join("samples");
            std::fs::create_dir_all(&samples_dir)?;
            let path = samples_dir.join("values.jsonl.gz");
            let mut catalog = read_catalog(&path)?;
            let ceiling = plan["full_ceiling"].as_i64().unwrap_or(100_000);
            let (done, failed) = sample_jobs(&project.dir, &todo, ceiling, now, &mut catalog)?;
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

/// Without the warehouse drivers, sampling can plan (`--dry-run`) but not run.
#[cfg(not(feature = "warehouse"))]
fn sample_jobs(
    _project_dir: &Path,
    _todo: &[&Value],
    _ceiling: i64,
    _now: chrono::DateTime<chrono::Utc>,
    _catalog: &mut Vec<Value>,
) -> Result<(usize, usize)> {
    bail!("this zsql build has no warehouse drivers; `zsql values sample --dry-run` shows the SQL, and the full build (cargo build --features warehouse) runs it")
}

/// Runs each sampling job against its datasource and folds the results
/// into the catalog; answers with how many succeeded and failed.
#[cfg(feature = "warehouse")]
fn sample_jobs(
    project_dir: &Path,
    todo: &[&Value],
    ceiling: i64,
    now: chrono::DateTime<chrono::Utc>,
    catalog: &mut Vec<Value>,
) -> Result<(usize, usize)> {
    let mut connections: std::collections::HashMap<String, Box<dyn zsql_warehouse::Warehouse>> = Default::default();
    let (mut done, mut failed) = (0, 0);
    for j in todo {
        let ds = j["datasource"].as_str().unwrap_or("").to_string();
        if !connections.contains_key(&ds) {
            let settings = zsql_warehouse::settings_for(project_dir, &ds)?;
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
            let distinct = wh
                .query(j["distinct_sql"].as_str().unwrap_or(""))?
                .first()
                .and_then(|r| r.first().cloned())
                .flatten()
                .and_then(|n| n.parse::<i64>().ok());
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
    Ok((done, failed))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn project_in(root: &Path) -> PathBuf {
        let dir = root.join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("project.yml"), "name: Proj\nuid: proj\n").unwrap();
        dir
    }

    /// The rule: here unless a flag names a server. `--server` and
    /// `--uid` do; `--branch` alone is a mistake, not a hint.
    #[test]
    fn where_a_command_runs() {
        let root = std::env::temp_dir().join(format!("zsql-where-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = project_in(&root);
        let here = Where { project: Some(dir.clone()), ..Default::default() };
        assert!(matches!(here.mode().unwrap(), Mode::Local(l) if l.project.uid == "proj"));
        assert!(
            matches!(Where { remote: true, branch: Some("dev".into()), ..here.clone() }.mode().unwrap(), Mode::Remote(t) if t.branch == "dev")
        );
        assert!(
            matches!(Where { server: Some("http://x".into()), branch: Some("dev".into()), ..here.clone() }.mode().unwrap(), Mode::Remote(t) if t.client.server == "http://x")
        );
        // --uid alone, with no project directory anywhere near, is a server's copy.
        assert!(
            matches!(Where { project: None, uid: Some("other".into()), ..Default::default() }.mode().unwrap(), Mode::Remote(t) if t.uid == "other" && t.branch == "main")
        );
        let err = match (Where { branch: Some("dev".into()), ..here.clone() }).mode() {
            Err(e) => e.to_string(),
            Ok(_) => panic!("--branch without -r was accepted"),
        };
        assert!(err.contains("add -r"), "{err}");
        // A project directory that is not a git repository still plans here.
        assert!(matches!(here.mode().unwrap(), Mode::Local(_)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn json_commands() {
        assert!(Command::Sql { query: QueryArgs { spec: None, expr: Some("a".into()), context: None }, explain: false, json: true }
            .wants_json());
        assert!(!Command::Tables.wants_json());
    }
}

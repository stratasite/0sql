//! zsql: author a semantic model, plan SQL from it, deploy it, serve it.
//!
//! One rule decides where a command runs. Anything that needs only the
//! model runs here, from the project directory at hand: sql, explain,
//! explore, fields, tables, values, check, test, the repl. Anything
//! about a server's copy needs `-r`: deploy, status, list, remove, and
//! the same reads against what the server holds. `--server` and
//! `--uid` imply `-r`; `ZSQL_SERVER` in the environment does not.

mod assist;
mod config;
mod deploy;
mod failure;
mod generate;
mod introspect;
mod local;
mod project;
mod remote;
mod repl;
mod scaffold;
#[cfg(feature = "server")]
mod serve;
mod show;
mod side;
mod templates;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use config::Project;
use local::Local;
use remote::Client;
use serde_json::Value;
use side::{Query, Session, Side};
use std::path::{Path, PathBuf};
use std::time::Instant;

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
    /// The server (implies -r); defaults to ZSQL_SERVER, .zsql, project.yml, then the local server.
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

impl Mode {
    /// The side to ask, loaded or connected.
    fn side(self) -> Result<Box<dyn Side>> {
        Ok(match self {
            Mode::Local(l) => Box::new(Session::quiet(l)?),
            Mode::Remote(t) => Box::new(t),
        })
    }
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

    /// The server a command about a server talks to, with the project's
    /// key when a project is at hand. No git is read for this.
    fn client(&self) -> Client {
        let project = self.project_dir().ok();
        Client::new(&config::server(self.server.as_deref(), project.as_ref()), config::api_key(project.as_ref()))
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
    /// Start a project here (or in DIR): project.yml, datasources.yml, models/, tests/, security.yml.
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
        what: scaffold::NewCommand,
    },
    /// Store the API key (and optionally a server) in .zsql for this project.
    Auth {
        #[arg(long)]
        api_key: String,
        #[arg(long)]
        server: Option<String>,
    },
    /// Deploy the checked-out branch: the model files, tests and sampled values (never secrets).
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
    /// Model files from catalogue/{datasource}.json: a draft per table, joins from its foreign keys. Never rewrites a file.
    Generate {
        /// Which datasource, by its key in datasources.yml; the only one when there is one.
        datasource: Option<String>,
        /// Only this schema (ClickHouse, MySQL: database).
        #[arg(long)]
        schema: Option<String>,
        /// Only this table, by physical name or name.
        #[arg(long)]
        table: Option<String>,
        /// For a table that has a model: add fields for these columns, comma-separated.
        #[arg(long, value_delimiter = ',')]
        columns: Vec<String>,
        /// For a table that has a model: add fields for every column it lacks.
        #[arg(long)]
        new: bool,
        /// Say what would be written and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Read a datasource's tables and columns into catalogue/{datasource}.json. Local only.
    Introspect {
        /// Which datasource, by its key in datasources.yml; the only one when there is one.
        datasource: Option<String>,
        /// Read one schema (ClickHouse: one database) instead of every non-system one.
        #[arg(long)]
        schema: Option<String>,
    },
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
    /// Print the query spec an expression line makes, and how each item was read.
    #[command(long_about = zsql_shorthand::ABOUT, after_long_help = zsql_shorthand::FORMS)]
    Spec {
        /// The line: projections, filters, calculations, segments and settings, comma-separated. Several arguments are joined with spaces, so quoting the line is optional.
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
    /// Sampled values of a dimension, from samples/values.jsonl.gz.
    Values { dim: String },
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
    /// Run the server here. Inside a project, serve that project and redeploy it as its files change.
    #[cfg(feature = "server")]
    Serve(serve::ServeArgs),
    /// The server in the background: start, stop, status, logs.
    #[cfg(feature = "server")]
    Server {
        #[command(subcommand)]
        what: serve::ServerCommand,
    },
}

impl Command {
    /// Whether the command answers in JSON, which is where a refusal
    /// goes too.
    fn wants_json(&self) -> bool {
        matches!(self, Command::Sql { json: true, .. } | Command::Explain { json: true, .. } | Command::Explore { json: true, .. })
    }

    /// Whether the command runs the server in this process.
    fn serves(&self) -> bool {
        #[cfg(feature = "server")]
        {
            matches!(self, Command::Serve(_))
        }
        #[cfg(not(feature = "server"))]
        {
            false
        }
    }
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
    // of panicking. The server must keep Rust's default of ignoring
    // SIGPIPE instead, or a client hanging up mid-answer would kill it.
    #[cfg(unix)]
    if !command.serves() {
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
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
        Command::Init { dir, name, uid } => scaffold::init(dir, name, uid),
        Command::New { what } => scaffold::new_file(what),
        Command::Auth { api_key, server } => {
            let project = at.project_dir()?;
            config::write_secret(&project.dir, "api_key", &api_key)?;
            if let Some(s) = server {
                config::write_secret(&project.dir, "server", &s)?;
            }
            println!("saved to {}", project.dir.join(config::LOCAL_FILE).display());
            Ok(())
        }
        Command::Deploy { dry_run, watch } => deploy::deploy(at, dry_run, watch),
        Command::Introspect { datasource, schema } => introspect::run(at.project.as_deref(), datasource.as_deref(), schema.as_deref()),
        Command::Generate { datasource, schema, table, columns, new, dry_run } => generate::run(
            at.project.as_deref(),
            generate::Options {
                datasource: datasource.as_deref(),
                schema: schema.as_deref(),
                table: table.as_deref(),
                columns: &columns,
                new,
                dry_run,
            },
        ),
        Command::Check => {
            let t0 = Instant::now();
            let mut side = at.mode()?.side()?;
            let v = side.check()?;
            let ok = show::print_deploy(&v);
            if side.origin() == "local" {
                eprintln!("-- local {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
            }
            if !ok {
                bail!("tests failed");
            }
            Ok(())
        }
        Command::Test => {
            if !show::print_tests(&at.mode()?.side()?.test()?) {
                bail!("tests failed");
            }
            Ok(())
        }
        Command::Sql { query, explain, json } => query.run(at, explain, json),
        Command::Explain { query, json } => query.run(at, true, json),
        Command::Explore { query, q, json } => {
            let (mode, query, context) = query.resolve(at)?;
            let mut side = mode.side()?;
            let v = side.explore(&query, context.as_ref(), q.as_deref())?;
            if json {
                show::print_json(&v)?;
            } else {
                show::print_explore(&v, &|s| s.to_string(), side.origin());
            }
            Ok(())
        }
        Command::Repl { context } => {
            let context = read_json(context)?;
            match at.mode()? {
                Mode::Local(l) => repl::run(Session::open(l)?, context),
                Mode::Remote(t) => repl::run(t, context),
            }
        }
        Command::Fields { q, hidden } => {
            show::print_fields(&at.mode()?.side()?.fields(&q.unwrap_or_default(), hidden)?);
            Ok(())
        }
        Command::Tables => {
            show::print_tables(&at.mode()?.side()?.tables()?);
            Ok(())
        }
        Command::Values { dim } => {
            show::print_values(&at.mode()?.side()?.values(&dim)?);
            Ok(())
        }
        Command::Status => {
            let t = at.target()?;
            show::print_json(&t.client.get(&remote::branch_path(&t.uid, &t.branch))?)
        }
        Command::List => {
            let v = at.client().get("/projects")?;
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
            let client = at.client();
            let v = client.get("/healthz")?;
            println!("{} {}", client.server, v.as_str().unwrap_or("ok"));
            Ok(())
        }
        #[cfg(feature = "server")]
        Command::Serve(args) => serve::serve(args, at),
        #[cfg(feature = "server")]
        Command::Server { what } => serve::server(what, at),
    }
}

/// What `zsql sql`, `zsql explain` and `zsql explore` take: the query, as
/// a spec file or an expression line, and the security context.
#[derive(clap::Args)]
struct QueryArgs {
    /// A JSON file holding a query spec.
    #[arg(long, conflicts_with = "expr", required_unless_present = "expr")]
    spec: Option<PathBuf>,
    /// An expression line: projections, filters, calculations, segments and settings, comma-separated. `zsql spec --help` has the forms.
    #[arg(long, required_unless_present = "spec")]
    expr: Option<String>,
    /// A JSON file holding the security context.
    #[arg(long)]
    context: Option<PathBuf>,
}

impl QueryArgs {
    /// Where to plan, and the query. The line is read by whoever plans
    /// it, here or on the server, so the item descriptions come from
    /// one place.
    fn resolve(self, at: Where) -> Result<(Mode, Query, Option<Value>)> {
        let mode = at.mode()?;
        let query = match (self.spec, self.expr) {
            (Some(path), _) => {
                let value = read_json(Some(path))?.expect("a path was given");
                Query::Spec(value.get("spec").cloned().unwrap_or(value))
            }
            (None, Some(line)) => Query::Expr(line),
            // clap requires one of the two.
            (None, None) => bail!("give the query as --spec FILE or --expr LINE"),
        };
        let context = read_json(self.context)?;
        Ok((mode, query, context))
    }

    fn run(self, at: Where, explain: bool, json: bool) -> Result<()> {
        let (mode, query, context) = self.resolve(at)?;
        let t0 = Instant::now();
        let mut side = mode.side()?;
        let v = side.sql(&query, context.as_ref(), explain)?;
        if json {
            show::print_json(&v)?;
        } else if explain {
            show::print_explain(&v, &|s| s.to_string(), side.origin());
        } else {
            for i in show::items(&v) {
                eprintln!("{i}");
            }
            for c in show::corrections(&v) {
                eprintln!("{c}");
            }
            println!("{}", show::datasource_line(&v));
            println!("{}", v["sql"].as_str().unwrap_or(""));
            if side.origin() == "local" {
                eprintln!("-- planned here in {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
            }
        }
        Ok(())
    }
}

/// A JSON file, named in any error: the read and the parse alike.
fn read_json(path: Option<PathBuf>) -> Result<Option<Value>> {
    Ok(match path {
        Some(p) => {
            let text = std::fs::read_to_string(&p).with_context(|| p.display().to_string())?;
            Some(serde_json::from_str(&text).with_context(|| format!("{}: not JSON", p.display()))?)
        }
        None => None,
    })
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

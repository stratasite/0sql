//! What `zsql` is told: the arguments, flag by flag, and where each
//! command runs. The help is written here, as the doc comments clap reads.

use crate::commands::datasource;
#[cfg(feature = "server")]
use crate::commands::serve;
use crate::config::{self, Project};
use crate::local::Local;
use crate::remote::Client;
use crate::side::{Query, Session, Side};
use crate::{show, tty};
use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub(crate) const GETTING_STARTED: &str = "\
A new project, start to finish:
  zsql init my-model && cd my-model     start here
  zsql datasource add                   point it at a warehouse (or edit datasources.yml)
  zsql catalog read --schema public     read what the warehouse holds into catalog/
  zsql generate                         pick tables, get a draft model
  zsql check                            make sure it loads
  zsql sample                           collect values so a question can name one
  zsql run 'revenue by region'          ask it something
Inside a project, `zsql` alone opens the repl. Every prompt has a flag form, so scripts never wait.";

#[derive(Parser)]
#[command(name = "zsql", about = "zsql: a semantic model, and the SQL it plans (0sql.io)", version, after_help = GETTING_STARTED)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
    /// Project directory; defaults to the one holding project.yml here or above.
    #[arg(long, global = true)]
    pub(crate) project: Option<PathBuf>,
    /// Never prompt: fail with the flag form instead. Implied when stdin is not a terminal or CI is set.
    #[arg(long, global = true)]
    pub(crate) no_input: bool,
    /// For a program driving zsql: JSON wherever a command has a JSON shape, and never a prompt.
    #[arg(long, global = true)]
    pub(crate) agent: bool,
    /// Log what zsql does, to stderr: -v for what happened, -vv for how, -vvv for every library too.
    #[arg(short = 'v', long = "verbose", global = true, action = clap::ArgAction::Count)]
    pub(crate) verbose: u8,
    /// What to log, in place of -v: a level, or levels by crate as in `warn,zsql_datasource=debug`. `zsql serve` logs at info unless told otherwise; every other command logs nothing.
    #[arg(long, global = true, env = "ZSQL_LOG", value_name = "FILTER")]
    pub(crate) log: Option<String>,
    /// How a log line is written: plain or json.
    #[arg(long, global = true, env = "ZSQL_LOG_FORMAT", value_name = "FORMAT", default_value = "plain")]
    pub(crate) log_format: String,
}

/// The flags that name a server's copy of the model, on the commands that
/// can ask either side.
#[derive(Args, Clone, Default)]
pub(crate) struct Remote {
    /// Ask the server's copy of the model rather than the files here.
    #[arg(short = 'r', long)]
    pub(crate) remote: bool,
    /// Branch on the server; defaults to the checked-out git branch. Implies nothing here: the files are what they are.
    #[arg(long)]
    pub(crate) branch: Option<String>,
    /// The server (implies -r); defaults to ZSQL_SERVER, .zsql, project.yml, then the local server.
    #[arg(long)]
    pub(crate) server: Option<String>,
    /// Project uid on the server (implies -r), when there is no project directory at hand.
    #[arg(long)]
    pub(crate) uid: Option<String>,
}

/// Which project a command works on, and whether here or on a server.
#[derive(Clone, Default)]
pub(crate) struct Where {
    pub(crate) project: Option<PathBuf>,
    pub(crate) remote: bool,
    pub(crate) branch: Option<String>,
    pub(crate) server: Option<String>,
    pub(crate) uid: Option<String>,
}

impl Where {
    pub(crate) fn here(project: Option<PathBuf>) -> Where {
        Where { project, ..Default::default() }
    }

    pub(crate) fn with(project: Option<PathBuf>, r: Remote) -> Where {
        Where { project, remote: r.remote, branch: r.branch, server: r.server, uid: r.uid }
    }
}

/// A server's copy of a project's branch.
pub(crate) struct Target {
    pub(crate) project: Option<Project>,
    pub(crate) uid: String,
    pub(crate) branch: String,
    pub(crate) client: Client,
}

/// Where a command runs.
pub(crate) enum Mode {
    Local(Local),
    Remote(Target),
}

impl Mode {
    /// The side to ask, loaded or connected.
    pub(crate) fn side(self) -> Result<Box<dyn Side>> {
        Ok(match self {
            Mode::Local(l) => Box::new(Session::quiet(l)?),
            Mode::Remote(t) => Box::new(t),
        })
    }
}

impl Where {
    pub(crate) fn wants_remote(&self) -> bool {
        self.remote || self.server.is_some() || self.uid.is_some()
    }

    /// The project directory at hand, unless the flags ask for a server.
    pub(crate) fn mode(&self) -> Result<Mode> {
        if self.wants_remote() {
            return Ok(Mode::Remote(self.target(false)?));
        }
        if let Some(b) = &self.branch {
            bail!("--branch {b} names a branch on the server; add -r to plan against it, or check it out to plan it here");
        }
        Ok(Mode::Local(Local { project: self.project_dir()? }))
    }

    pub(crate) fn project_dir(&self) -> Result<Project> {
        match &self.project {
            Some(dir) => config::read_project(dir),
            None => config::find_project(Path::new(".")),
        }
    }

    /// The server a command about a server talks to, with the project's
    /// key when a project is at hand. No git is read for this.
    pub(crate) fn client(&self) -> Client {
        let project = self.project_dir().ok();
        Client::new(&config::server(self.server.as_deref(), project.as_ref()), config::api_key(project.as_ref()))
    }

    /// The server target, for commands that are about a server's copy.
    /// This is the one place the git branch is read. A read that cannot
    /// learn the branch is told to name one; only a deploy is told to
    /// commit, since only a deploy sends the checked-out branch.
    pub(crate) fn target(&self, deploying: bool) -> Result<Target> {
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
            (None, Some(p)) => match config::git_branch(&p.dir) {
                Ok(b) => b,
                Err(e) if deploying => return Err(e),
                Err(_) => bail!("no branch to ask for: {} has no committed branch; add --branch NAME, or commit first", p.dir.display()),
            },
            (None, None) => "main".into(),
        };
        let server = config::server(self.server.as_deref(), project.as_ref());
        let api_key = config::api_key(project.as_ref());
        Ok(Target { project, uid, branch, client: Client::new(&server, api_key) })
    }
}

#[derive(Subcommand)]
pub(crate) enum Command {
    // ----------------------------------------------------- getting started
    /// Start a project here (or in DIR): project.yml, datasources.yml, models/, tests/, AGENTS.md.
    #[command(display_order = 1)]
    Init {
        /// Where to start it; here when unsaid.
        dir: Option<PathBuf>,
        /// The project's name; the directory's when unsaid.
        #[arg(long)]
        name: Option<String>,
        /// The uid the server knows the project as; made from the name when unsaid.
        #[arg(long)]
        uid: Option<String>,
        /// Do not run `git init` in a directory that is not a repository.
        #[arg(long)]
        no_git: bool,
    },
    /// The warehouses in datasources.yml: list them, add one, test a connection.
    #[command(display_order = 2)]
    Datasource {
        #[command(subcommand)]
        what: datasource::DatasourceWhat,
    },
    /// What the warehouse holds, in catalog/{datasource}.json: read it, sync it, search it, show a table. Local only.
    #[command(display_order = 3, alias = "introspect")]
    Catalog {
        #[command(subcommand)]
        what: Option<CatalogWhat>,
        /// Which datasource, by its key in datasources.yml; the only one when there is one.
        #[arg(short = 'd', long)]
        datasource: Option<String>,
        /// Print what the catalog holds as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Draft model files from the catalog for the tables you pick. Never rewrites a file.
    #[command(
        display_order = 4,
        after_help = "With no table and a terminal, pick from a list; without a terminal, name the tables, a --schema, or --all."
    )]
    Generate {
        /// Tables to draft, as schema.table or table; with none, a picker on a terminal. A word ending in `/` is the directory to draft into.
        tables: Vec<String>,
        /// The models/{dir} the files go in; each table's own schema when unsaid. The directories are the model's subject areas and need not mirror the warehouse.
        #[arg(long)]
        dir: Option<String>,
        /// Which datasource, by its key in datasources.yml; the only one when there is one.
        #[arg(short = 'd', long)]
        datasource: Option<String>,
        /// Every table of this schema (ClickHouse, MySQL: database).
        #[arg(long)]
        schema: Option<String>,
        /// Every table in the catalog. Never the default.
        #[arg(long, conflicts_with = "schema")]
        all: bool,
        /// For a named table that has a model: add fields for these columns, comma-separated.
        #[arg(long, value_delimiter = ',', requires = "tables")]
        columns: Vec<String>,
        /// For a table that has a model: add fields for every column it lacks.
        #[arg(long, alias = "new")]
        new_columns: bool,
        /// A named table the catalog lacks is read from the warehouse first and added to the catalog.
        #[arg(long)]
        live: bool,
        /// Draft a table that already has a model again, replacing the file and whatever was edited into it. Asked for on a terminal when the table is named.
        #[arg(long)]
        overwrite: bool,
        /// An empty model file for the table named, from the template and no catalog: `zsql generate orders --empty`, or `help/sessions` to say the directory too.
        #[arg(long)]
        empty: bool,
        /// The joins file for a set of tables, at models/{domain}/rel.{name}.yml: the template, or --infer to fill it in. Takes {domain}/{name}, {domain}, or nothing for every domain with tables.
        #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "DOMAIN/NAME", conflicts_with = "empty", alias = "rel")]
        relationships: Option<String>,
        /// With --relationships: infer the joins from the table files there, by matching key column names. No query is run.
        #[arg(long, requires = "relationships")]
        infer: bool,
        /// With --empty: the physical name; the table's name when unsaid.
        #[arg(long, requires = "empty")]
        physical_name: Option<String>,
        /// With --empty: the models/{domain} directory; core when unsaid.
        #[arg(long, requires = "empty")]
        domain: Option<String>,
        /// Say what would be written and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Load the model, check it against the catalog, and run its tests. Connects to nothing (`datasource test` does); with -r, on the server.
    #[command(display_order = 5)]
    Check {
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        remote: Remote,
    },
    /// Collect the values of the dimensions marked enable_sampling into samples/values.jsonl.gz. Local only; spends warehouse time.
    #[command(display_order = 6)]
    Sample {
        /// Only these dimensions or tables, by name or uid; every sampled dimension when none.
        names: Vec<String>,
        /// Which datasource; every one when unsaid.
        #[arg(short = 'd', long)]
        datasource: Option<String>,
        /// Say what would be run and run nothing.
        #[arg(long)]
        dry_run: bool,
        /// Refuse before running when more queries than this are needed; 100 when unsaid.
        #[arg(long)]
        budget: Option<usize>,
        /// Run without asking.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Report the plan and the outcome as JSON. Counts only; the values go to the samples file.
        #[arg(long)]
        json: bool,
    },
    /// Run a query on the datasource the planner chooses and keep its rows under ~/.zsql/results. Local only.
    #[command(display_order = 7)]
    Run {
        #[command(flatten)]
        query: QueryArgs,
        /// Keep at most this many rows, pushed into the statement; the default cap is 10,000.
        #[arg(long, conflicts_with = "all")]
        limit: Option<u64>,
        /// No limit: every row, held in memory.
        #[arg(long)]
        all: bool,
        /// Run it even when a result within its TTL is stored, and replace that result.
        #[arg(long)]
        fresh: bool,
        /// Statement timeout in seconds for this run.
        #[arg(long)]
        timeout: Option<u64>,
        /// table (default), json, jsonl or csv; never changes with where stdout goes.
        #[arg(long)]
        format: Option<String>,
        /// Write the rows to this file, in the format its extension says (json, jsonl, csv).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print at most this many rows; a table on a terminal shows 50 unless said.
        #[arg(long)]
        head: Option<usize>,
        /// No footer, no warnings.
        #[arg(long)]
        quiet: bool,
    },
    // ---------------------------------------------------- asking the model
    /// The SQL for a line or a spec; with -r, planned by the server.
    #[command(display_order = 10)]
    Sql {
        #[command(flatten)]
        query: QueryArgs,
        /// Explain the plan instead: node graph and timings, as `zsql explain`.
        #[arg(long)]
        explain: bool,
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        remote: Remote,
    },
    /// How a query is planned: SQL, node graph and timings.
    #[command(display_order = 11)]
    Explain {
        #[command(flatten)]
        query: QueryArgs,
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        remote: Remote,
    },
    /// What a query can take on: the dimensions to drill into and the measures to add.
    #[command(display_order = 12)]
    Explore {
        #[command(flatten)]
        query: QueryArgs,
        /// Keep only fields whose name, uid or synonyms contain this, or come close.
        #[arg(long, short)]
        q: Option<String>,
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        remote: Remote,
    },
    /// The fields of the model, filtered by text.
    #[command(display_order = 13)]
    Fields {
        /// Keep fields whose name, uid or synonyms contain this.
        term: Option<String>,
        /// Include hidden fields.
        #[arg(long)]
        hidden: bool,
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        remote: Remote,
    },
    /// The tables of the model.
    #[command(display_order = 14)]
    Tables {
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        remote: Remote,
    },
    /// A dimension's sampled values, from samples/values.jsonl.gz.
    #[command(display_order = 15)]
    Values {
        /// The dimension, by name or uid.
        dim: String,
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        remote: Remote,
    },
    /// What `zsql run` kept: list, show, export, the SQL, remove, purge.
    #[command(display_order = 16)]
    Results {
        #[command(subcommand)]
        what: Option<ResultsWhat>,
    },
    /// All of the above, interactively: the model here, reloaded as its files change; with -r, the server's copy.
    #[command(display_order = 17)]
    Repl {
        /// A JSON file holding the security context to plan under.
        #[arg(long)]
        context: Option<PathBuf>,
        #[command(flatten)]
        remote: Remote,
    },
    /// The query spec an expression line makes, and how each item was read.
    #[command(display_order = 18, long_about = zsql_shorthand::ABOUT, after_long_help = zsql_shorthand::FORMS)]
    Spec {
        /// The line: projections, filters, calculations, segments and settings, comma-separated. Several arguments are joined with spaces, so quoting the line is optional.
        #[arg(num_args = 1.., value_name = "LINE", required = true)]
        line: Vec<String>,
    },
    // --------------------------------------------------------- shipping it
    /// Run tests/*.yml here; with -r, the deployed branch's tests on the server. `--new NAME` writes one from the template.
    #[command(display_order = 30)]
    Test {
        /// Write tests/{name}.yml from the template, projecting the model's first fields, and run nothing.
        #[arg(long, value_name = "NAME")]
        new: Option<String>,
        /// Print the results as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        remote: Remote,
    },
    /// Save the server's deploy token, and optionally the server, in .zsql for this project.
    #[command(display_order = 31)]
    Auth {
        /// The deploy token the server was started with (its ZSQL_DEPLOY_TOKEN). A server without one needs none.
        #[arg(long)]
        api_key: String,
        /// The server the token is for; the default server when unsaid.
        #[arg(long)]
        server: Option<String>,
    },
    /// Deploy the checked-out branch: the model files, tests and sampled values (never secrets).
    #[command(display_order = 32)]
    Deploy {
        /// Validate only; the server keeps nothing.
        #[arg(long)]
        dry_run: bool,
        /// Redeploy whenever a project file changes.
        #[arg(long)]
        watch: bool,
        /// Send without loading and testing the model here first.
        #[arg(long)]
        skip_check: bool,
        #[command(flatten)]
        remote: Remote,
    },
    /// The deployed branch's summary.
    #[command(display_order = 33)]
    Status {
        #[command(flatten)]
        remote: Remote,
    },
    /// Every deployment on the server.
    #[command(display_order = 34, alias = "list")]
    Deployments {
        #[command(flatten)]
        remote: Remote,
    },
    /// Remove a deployed branch from the server.
    #[command(display_order = 35, alias = "remove")]
    Undeploy {
        /// Confirm: this removes the branch from the server.
        #[arg(long, short = 'y')]
        yes: bool,
        #[command(flatten)]
        remote: Remote,
    },
    // ---------------------------------------------------- running a server
    /// Run the server here, in the foreground. Inside a project, serve that project and redeploy it as its files change.
    #[cfg(feature = "server")]
    #[command(display_order = 40)]
    Serve(serve::ServeArgs),
    /// The server in the background: start, stop, status, logs, health.
    #[cfg(feature = "server")]
    #[command(display_order = 41)]
    Server {
        #[command(subcommand)]
        what: serve::ServerCommand,
    },
    /// Liveness of a server, by its URL.
    #[command(hide = true)]
    Health {
        #[command(flatten)]
        remote: Remote,
    },
    /// Shell completion for bash, zsh or fish; `source <(zsql completions bash)`.
    #[command(display_order = 42)]
    Completions {
        /// bash, zsh, fish, elvish or powershell.
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand)]
pub(crate) enum CatalogWhat {
    /// The first read, or more of it: named schemas and tables are read and merged into the file. With nothing named: a schema picker on a terminal.
    Read {
        /// Tables to read by name, as schema.table or table.
        tables: Vec<String>,
        /// Read this schema whole (ClickHouse, MySQL: this database); repeatable.
        #[arg(long)]
        schema: Vec<String>,
        /// Every non-system schema. Never the default.
        #[arg(long)]
        all: bool,
        /// Which datasource, by its key in datasources.yml; the only one when there is one.
        #[arg(short = 'd', long)]
        datasource: Option<String>,
        /// Print what was read as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Re-read every schema and table the file records, report drift, drop what the warehouse says is gone.
    Sync {
        /// Which datasource, by its key in datasources.yml; the only one when there is one.
        #[arg(short = 'd', long)]
        datasource: Option<String>,
        /// Print what changed as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Tables whose name holds the pattern: in the file, or in the warehouse with --live.
    Search {
        /// Part of a table name, matched without regard to case.
        pattern: String,
        /// Ask the warehouse, every non-system schema, rather than the file.
        #[arg(long)]
        live: bool,
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
        /// Which datasource, by its key in datasources.yml; the only one when there is one.
        #[arg(short = 'd', long)]
        datasource: Option<String>,
    },
    /// One table's columns, keys and comment, from the file: every datasource's when -d is unsaid.
    Show {
        /// The table, as schema.table or table.
        table: String,
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
        /// Which datasource; every one's catalog is searched when unsaid.
        #[arg(short = 'd', long)]
        datasource: Option<String>,
    },
    /// Read these tables by name and add them to the file. The same as `read TABLE...`.
    #[command(hide = true)]
    Add {
        tables: Vec<String>,
        #[arg(short = 'd', long)]
        datasource: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum ResultsWhat {
    /// Every stored result, newest first.
    List {
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
    },
    /// One result's rows.
    Show {
        /// The result's id, or enough of it to be one.
        id: String,
        /// table (default), json, jsonl or csv.
        #[arg(long)]
        format: Option<String>,
        /// Print at most this many rows.
        #[arg(long)]
        head: Option<usize>,
        /// Skip this many rows first.
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// One result's rows to a file, in the format its extension says.
    Export {
        /// The result's id, or enough of it to be one.
        id: String,
        /// The file to write; .json, .jsonl or .csv.
        #[arg(long)]
        out: PathBuf,
    },
    /// The statement a result ran.
    Sql {
        /// The result's id, or enough of it to be one.
        id: String,
    },
    /// Remove results by id.
    Rm {
        /// Ids, or enough of each to be one.
        ids: Vec<String>,
    },
    /// Remove every result of this project.
    Purge {
        /// Remove without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

impl Command {
    /// Whether the command answers in JSON, which is where a refusal
    /// goes too.
    pub(crate) fn wants_json(&self) -> bool {
        if tty::agent() {
            return true;
        }
        match self {
            Command::Sql { json, .. }
            | Command::Explain { json, .. }
            | Command::Explore { json, .. }
            | Command::Fields { json, .. }
            | Command::Tables { json, .. }
            | Command::Values { json, .. } => *json,
            Command::Run { format, .. } => format.as_deref().is_some_and(|f| f.eq_ignore_ascii_case("json")),
            Command::Results { what: Some(ResultsWhat::Show { format, .. }) } => {
                format.as_deref().is_some_and(|f| f.eq_ignore_ascii_case("json"))
            }
            Command::Results { what: Some(ResultsWhat::List { json }) } => *json,
            Command::Check { json, .. } | Command::Test { json, .. } | Command::Sample { json, .. } => *json,
            Command::Catalog { what: Some(CatalogWhat::Search { json, .. }), .. }
            | Command::Catalog { what: Some(CatalogWhat::Show { json, .. }), .. }
            | Command::Catalog { what: Some(CatalogWhat::Read { json, .. }), .. }
            | Command::Catalog { what: Some(CatalogWhat::Sync { json, .. }), .. } => *json,
            Command::Catalog { what: None, json, .. } => *json,
            // The flag on the parent reaches a subcommand that has its own.
            Command::Catalog { json: true, .. } => true,
            Command::Datasource { what: datasource::DatasourceWhat::List { json } } => *json,
            #[cfg(feature = "server")]
            Command::Server { what: serve::ServerCommand::Status { json, .. } } => *json,
            _ => false,
        }
    }

    /// Whether the command runs the server in this process.
    pub(crate) fn serves(&self) -> bool {
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

/// spec file, and the security context.
/// What `zsql sql`, `zsql explain`, `zsql explore` and `zsql run` take:
/// the query as an expression line (the positional words, joined) or a
#[derive(clap::Args)]
pub(crate) struct QueryArgs {
    /// The line: projections, filters, calculations, segments and settings, comma-separated. Several arguments are joined with spaces, so quoting is optional. `zsql spec --help` has the forms.
    #[arg(num_args = 1.., value_name = "LINE", conflicts_with = "spec")]
    pub(crate) line: Vec<String>,
    /// A JSON file holding a query spec, instead of a line.
    #[arg(long)]
    pub(crate) spec: Option<PathBuf>,
    /// The line as one flag; the same as giving it bare.
    #[arg(long, hide = true, conflicts_with_all = ["spec", "line"])]
    pub(crate) expr: Option<String>,
    /// A JSON file holding the security context to plan under.
    #[arg(long)]
    pub(crate) context: Option<PathBuf>,
}

impl QueryArgs {
    /// Where to plan, and the query. The line is read by whoever plans
    /// it, here or on the server, so the item descriptions come from
    /// one place.
    pub(crate) fn resolve(self, at: Where) -> Result<(Mode, Query, Option<Value>)> {
        let mode = at.mode()?;
        let line = self.expr.or_else(|| (!self.line.is_empty()).then(|| self.line.join(" ")));
        let query = match (self.spec, line) {
            (Some(path), _) => {
                let value = read_json(Some(path))?.expect("a path was given");
                Query::Spec(value.get("spec").cloned().unwrap_or(value))
            }
            (None, Some(line)) => Query::Expr(line),
            (None, None) => {
                // A usage error, as clap would report one: exit 2.
                use clap::CommandFactory;
                Cli::command()
                    .error(
                        clap::error::ErrorKind::MissingRequiredArgument,
                        "give the query: a line such as 'revenue by region', or --spec FILE",
                    )
                    .exit()
            }
        };
        let context = read_json(self.context)?;
        Ok((mode, query, context))
    }

    pub(crate) fn run(self, at: Where, explain: bool, json: bool) -> Result<()> {
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
pub(crate) fn read_json(path: Option<PathBuf>) -> Result<Option<Value>> {
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
        // A project directory that is not a git repository still plans here,
        // and a read against the server is told to name a branch, not to commit.
        assert!(matches!(here.mode().unwrap(), Mode::Local(_)));
        let err = Where { remote: true, ..here.clone() }.mode().err().unwrap().to_string();
        assert!(err.contains("add --branch NAME") && !err.contains("deploy"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn json_commands() {
        let q = || QueryArgs { line: vec!["a".into()], spec: None, expr: None, context: None };
        assert!(Command::Sql { query: q(), explain: false, json: true, remote: Remote::default() }.wants_json());
        assert!(Command::Run {
            query: q(),
            limit: None,
            all: false,
            fresh: false,
            timeout: None,
            format: Some("json".into()),
            out: None,
            head: None,
            quiet: false
        }
        .wants_json());
        assert!(!Command::Tables { json: false, remote: Remote::default() }.wants_json());
    }

    #[test]
    fn the_command_line_parses_its_own_help() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}

mod exprs;
mod remote;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Instant;
use zsql_dialect::Dialects;
use zsql_golden::{print_scoreboard, score, Fixtures, PathSource};
use zsql_semantic::{Snapshot, Universe};
use zsql_spec::SecurityContext;
use serde_yaml as serde_yaml_value;

#[derive(Parser)]
#[command(name = "zsql", about = "zsql: semantic layer to SQL (0sql.io)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Where the semantic model comes from: a snapshot file, or a project
/// directory (project.yml, datasources.yml, models/).
#[derive(Args)]
struct Source {
    /// A snapshot JSON file (`.json` or `.json.gz`).
    #[arg(long, conflicts_with = "project")]
    snapshot: Option<PathBuf>,
    /// A project directory, loaded and formed on the fly.
    #[arg(long)]
    project: Option<PathBuf>,
    /// Directory of dwh dialect settings; the settings compiled in are used by default.
    #[arg(long)]
    dialects: Option<PathBuf>,
}

struct Model {
    snapshot: Snapshot,
    warnings: Vec<String>,
    tests: Vec<zsql_project::ProjectTest>,
    values: Option<zsql_project::values::ValueCatalog>,
    dialects: Dialects,
}

impl Source {
    fn load(&self) -> Result<Model> {
        let dialects = match &self.dialects {
            Some(dir) => Dialects::new(dir),
            None => Dialects::builtin(),
        };
        match (&self.snapshot, &self.project) {
            (Some(path), _) => Ok(Model { snapshot: Snapshot::from_path(path).with_context(|| path.display().to_string())?, warnings: vec![], tests: vec![], values: None, dialects }),
            (None, Some(dir)) => {
                let loaded = zsql_project::load(dir, &dialects)?;
                Ok(Model { snapshot: loaded.snapshot, warnings: loaded.warnings, tests: loaded.tests, values: loaded.values, dialects })
            }
            (None, None) => bail!("give --snapshot <file> or --project <dir>"),
        }
    }
}

fn summary(snap: &Snapshot) -> String {
    format!(
        "{} / {}: {} datasources, {} tables, {} fields, {} expressions, {} joins, {} paths, {} policies",
        snap.project.uid,
        snap.branch.uid,
        snap.datasources.len(),
        snap.tables.len(),
        snap.fields.len(),
        snap.expressions.len(),
        snap.join_defs.len(),
        snap.paths.len(),
        snap.security_policies.len()
    )
}

fn read_context(path: Option<PathBuf>) -> Result<Option<SecurityContext>> {
    Ok(match path {
        Some(p) => Some(serde_json::from_str(&std::fs::read_to_string(&p).with_context(|| p.display().to_string())?)?),
        None => None,
    })
}

#[derive(Subcommand)]
enum Command {
    /// Load the model, build the universe and report what is in it.
    Check {
        #[command(flatten)]
        source: Source,
    },
    /// Plan one query against the model and print the SQL.
    Plan {
        #[command(flatten)]
        source: Source,
        /// A JSON file holding a query spec (fields by uid or name), or a golden case (its `spec` is used).
        #[arg(long, conflicts_with = "expr")]
        spec: Option<PathBuf>,
        /// A comma-separated line of projections, filters and calculations, as the repl reads it.
        #[arg(long)]
        expr: Option<String>,
        /// A JSON file holding the security context (email, admin flags, groups with tags).
        #[arg(long)]
        context: Option<PathBuf>,
        /// Also print each node's alias and the identity string it hashes from.
        #[arg(long)]
        identity: bool,
    },
    /// Write the loaded model as a snapshot file (`.json`, or `.json.gz` when the name ends in .gz).
    Export {
        #[command(flatten)]
        source: Source,
        #[arg(long)]
        out: PathBuf,
    },
    /// Run the project's tests/*.yml planner assertions.
    Test {
        #[command(flatten)]
        source: Source,
    },
    /// Plan interactively: a comma-separated line of projections, filters and calculations,
    /// read from their shape; `.help` lists the forms.
    Repl {
        #[command(flatten)]
        source: Source,
        #[arg(long)]
        context: Option<PathBuf>,
    },
    /// Score the planner against the golden cases.
    Golden {
        #[arg(long, default_value = "")]
        filter: String,
        #[arg(long)]
        strict: bool,
        #[arg(long)]
        fixtures: Option<PathBuf>,
        /// Write expected and produced SQL of every failing case into this directory.
        #[arg(long)]
        dump: Option<PathBuf>,
        /// Join paths to plan over: `formed` (computed here) or `snapshot` (the stored paths).
        #[arg(long, default_value = "formed")]
        paths: String,
    },
    /// Time each phase over the golden cases: loading, formation, lowering and planning.
    Bench {
        #[arg(long)]
        fixtures: Option<PathBuf>,
        /// Plan each case this many times.
        #[arg(long, default_value_t = 5)]
        rounds: usize,
    },
    /// Form join paths for the model and, for a snapshot, compare them with the paths it stores.
    Form {
        #[command(flatten)]
        source: Source,
    },
    /// Deploy the project directory to a zsqld server (model files and sampled values, no secrets).
    Deploy {
        #[arg(long)]
        project: PathBuf,
        /// The zsqld server.
        #[arg(long, default_value = remote::DEFAULT_SERVER)]
        server: String,
        /// Project uid; defaults to project.yml's uid.
        #[arg(long)]
        uid: Option<String>,
        #[arg(long, default_value = "main")]
        branch: String,
    },
    /// Talk to a running zsqld: plan, discover, list deployments.
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Sample dimension values from the project's warehouses into values.jsonl.gz, or show them.
    Values {
        #[command(subcommand)]
        command: ValuesCommand,
    },
}

/// Which deployment a remote command addresses.
#[derive(Args)]
struct Target {
    /// The zsqld server.
    #[arg(long, default_value = remote::DEFAULT_SERVER)]
    server: String,
    /// Project uid; defaults to the uid in --project's project.yml, or ./project.yml.
    #[arg(long)]
    uid: Option<String>,
    #[arg(long)]
    project: Option<PathBuf>,
    #[arg(long, default_value = "main")]
    branch: String,
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// Liveness of the server.
    Health {
        #[arg(long, default_value = remote::DEFAULT_SERVER)]
        server: String,
    },
    /// Every deployment on the server.
    List {
        #[arg(long, default_value = remote::DEFAULT_SERVER)]
        server: String,
    },
    /// Summary and warnings of one deployment.
    Show {
        #[command(flatten)]
        target: Target,
    },
    /// Plan on the server: a spec file or a shorthand line.
    Plan {
        #[command(flatten)]
        target: Target,
        #[arg(long, conflicts_with = "expr")]
        spec: Option<PathBuf>,
        #[arg(long)]
        expr: Option<String>,
        /// A JSON file holding the security context.
        #[arg(long)]
        context: Option<PathBuf>,
        /// Print the whole response, timings and identities included.
        #[arg(long)]
        json: bool,
    },
    /// Fields of the deployment, filtered by text.
    Fields {
        #[command(flatten)]
        target: Target,
        q: Option<String>,
        #[arg(long)]
        hidden: bool,
    },
    /// Tables of the deployment.
    Tables {
        #[command(flatten)]
        target: Target,
    },
    /// Sampled values of a dimension.
    Values {
        #[command(flatten)]
        target: Target,
        dim: String,
    },
    /// The interactive loop, planned on the server.
    Repl {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        context: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ValuesCommand {
    /// Sample the listable dimensions (top values by row count, or ranges for numbers and dates).
    Sample {
        #[arg(long)]
        project: PathBuf,
        /// Only these dimensions (uids or names); default: every listable one.
        #[arg(long, value_delimiter = ',')]
        dims: Vec<String>,
        /// Take every distinct value, not the top `value_list_size` (refused above 100k).
        #[arg(long)]
        full: bool,
        /// Resample only dimensions sampled more than this many days ago (or never).
        #[arg(long)]
        older_than: Option<i64>,
        /// Print the SQL that would run and stop.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show what values.jsonl.gz holds, for one dimension or all.
    Show {
        #[arg(long)]
        project: PathBuf,
        /// A dimension uid or name.
        dim: Option<String>,
    },
}

fn main() -> Result<()> {
    // A closed pipe (`zsql ... | head`) ends the program quietly instead of panicking.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    match Cli::parse().command {
        Command::Check { source } => {
            let model = source.load()?;
            println!("{}", summary(&model.snapshot));
            let universe = Universe::build(model.snapshot)?;
            for w in model.warnings.iter().chain(universe.warnings().iter()) {
                println!("warning: {w}");
            }
        }
        Command::Plan { source, spec, expr, context, identity } => {
            let model = source.load()?;
            for w in &model.warnings {
                eprintln!("warning: {w}");
            }
            let universe = Universe::build(model.snapshot)?;
            let context = read_context(context)?;
            let base = model.dialects.base()?;
            let spec = match (spec, expr) {
                (Some(path), _) => {
                    let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
                    let value: serde_json::Value = serde_json::from_str(&text)?;
                    let value = value.get("spec").cloned().unwrap_or(value);
                    zsql_spec::Spec::from_value(value)?
                }
                (None, Some(line)) => {
                    let (spec, items) = exprs::parse_line(&line).map_err(|e| anyhow::anyhow!(e))?;
                    for d in exprs::describe(&items) {
                        eprintln!("{d}");
                    }
                    spec
                }
                (None, None) => bail!("plan needs --spec or --expr"),
            };
            let query = zsql_spec::lower(&spec, &universe, &base)?;
            let plan = zsql_planner::Planner::new(&universe, &model.dialects, None).plan_query_explained(query, context.as_ref(), identity)?;
            println!("{}", plan.sql);
            if identity {
                for (alias, id) in &plan.identities {
                    println!("\n== {alias}\n{id}");
                }
            }
        }
        Command::Export { source, out } => {
            let model = source.load()?;
            for w in &model.warnings {
                eprintln!("warning: {w}");
            }
            let text = serde_json::to_string_pretty(&model.snapshot)?;
            if out.extension().and_then(|e| e.to_str()) == Some("gz") {
                let file = std::fs::File::create(&out).with_context(|| out.display().to_string())?;
                let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
                enc.write_all(text.as_bytes())?;
                enc.finish()?;
            } else {
                std::fs::write(&out, text).with_context(|| out.display().to_string())?;
            }
            println!("wrote {} ({})", out.display(), summary(&model.snapshot));
        }
        Command::Test { source } => {
            let model = source.load()?;
            if model.tests.is_empty() {
                println!("no tests/*.yml in the project");
                return Ok(());
            }
            let universe = Universe::build(model.snapshot)?;
            let planner = zsql_planner::Planner::new(&universe, &model.dialects, None);
            let base = model.dialects.base()?;
            let normalize = |sql: &str| sql.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
            let (mut passed, mut failed) = (0, 0);
            for t in &model.tests {
                let spec = zsql_spec::Spec {
                    projections: t.projections.iter().map(|f| zsql_spec::spec::ProjectionSpec { field: Some(serde_json::Value::String(f.clone())), ..Default::default() }).collect(),
                    ..Default::default()
                };
                let outcome = zsql_spec::lower(&spec, &universe, &base)
                    .map_err(|e| e.to_string())
                    .and_then(|q| planner.plan_query(q, None).map(|p| p.sql).map_err(|e| e.to_string()));
                let verdict = match (&outcome, &t.assert_sql, &t.assert_regex) {
                    (Err(e), _, _) => Err(format!("error: {e}")),
                    (Ok(sql), Some(expected), _) => {
                        if normalize(sql) == normalize(expected) {
                            Ok(())
                        } else {
                            Err(format!("Generated SQL does not match expected SQL\n--- expected\n{expected}\n--- generated\n{sql}"))
                        }
                    }
                    (Ok(sql), None, Some(pattern)) => match regex::RegexBuilder::new(pattern).case_insensitive(true).dot_matches_new_line(true).build() {
                        Ok(re) if re.is_match(sql) => Ok(()),
                        Ok(_) => Err(format!("Generated SQL does not match regex pattern {pattern}\n--- generated\n{sql}")),
                        Err(e) => Err(format!("bad assert_regex: {e}")),
                    },
                    (Ok(_), None, None) => Err("No assertion defined (assert_sql or assert_regex required)".into()),
                };
                match verdict {
                    Ok(()) => {
                        passed += 1;
                        println!("PASS {}", t.name);
                    }
                    Err(msg) => {
                        failed += 1;
                        println!("FAIL {} ({})\n{}", t.name, t.file.display(), msg);
                    }
                }
            }
            println!("{passed} passed, {failed} failed");
            if failed > 0 {
                bail!("{failed} test(s) failed");
            }
        }
        Command::Repl { source, context } => {
            let t = Instant::now();
            let model = source.load()?;
            for w in &model.warnings {
                eprintln!("warning: {w}");
            }
            let universe = Universe::build(model.snapshot.clone())?;
            let context = read_context(context)?;
            let planner = zsql_planner::Planner::new(&universe, &model.dialects, None);
            let base = model.dialects.base()?;
            let tty = std::io::stdout().is_terminal();
            let dim = |s: &str| if tty { format!("\x1b[2m{s}\x1b[0m") } else { s.to_string() };
            let bold = |s: &str| if tty { format!("\x1b[1m{s}\x1b[0m") } else { s.to_string() };
            println!("{}", dim(&format!("{} in {:.0} ms", summary(universe.snapshot()), t.elapsed().as_secs_f64() * 1e3)));
            println!("{}", dim("type a line of projections and filters; .help for the forms, .quit to leave"));

            let mut editor: Option<rustyline::DefaultEditor> = if std::io::stdin().is_terminal() { rustyline::DefaultEditor::new().ok() } else { None };
            let history = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".zsql_history"));
            if let (Some(ed), Some(h)) = (editor.as_mut(), &history) {
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
                // Commands carry a leading dot, as in sqlite, so no field name can shadow them; the
                // bare `help` and `quit` stay usable while no field is named that.
                let (word, rest) = line.split_once(' ').map(|(c, r)| (c, r.trim())).unwrap_or((line, ""));
                let names_a_field = |w: &str| universe.fields().iter().any(|f| f.name.eq_ignore_ascii_case(w) || f.uid.eq_ignore_ascii_case(w));
                let cmd = match word.strip_prefix('.') {
                    Some(c) => c,
                    None if matches!(word, "help" | "?" | "quit" | "exit" | "q") && rest.is_empty() && !names_a_field(word) => word,
                    None => "",
                };
                match cmd {
                    "quit" | "exit" | "q" => break,
                    "help" | "?" => println!("{}", exprs::HELP),
                    "tables" => {
                        for t in universe.tables() {
                            let ds = universe.datasource(t.datasource_id).map(|d| d.uid.as_str()).unwrap_or("?");
                            println!("{:<32} {:<32} {} cost {}", t.uid, t.physical_name, ds, t.cost);
                        }
                    }
                    "fields" => {
                        let needle = rest.to_lowercase();
                        for f in universe.fields() {
                            if needle.is_empty() || f.uid.to_lowercase().contains(&needle) || f.name.to_lowercase().contains(&needle) {
                                let kind = match f.kind {
                                    zsql_semantic::FieldKind::Dimension => "dim",
                                    zsql_semantic::FieldKind::Measure => "msr",
                                };
                                println!("{:<40} {} {}{}", f.uid, kind, f.name, if f.hidden { " (hidden)" } else { "" });
                            }
                        }
                    }
                    "values" => match &model.values {
                        None => println!("no values.jsonl.gz in the project; run `zsql values sample --project <dir>`"),
                        Some(cat) => {
                            let needle = rest.to_lowercase();
                            for d in cat.dimensions.iter().filter(|d| needle.is_empty() || d.uid.to_lowercase().contains(&needle) || d.name.to_lowercase().contains(&needle)) {
                                print_values(d, needle.is_empty());
                            }
                        }
                    },
                    "tojson" => {
                        // The shorthand line as the JSON spec it lowers from, not planned.
                        match exprs::parse_line(rest) {
                            Ok((spec, items)) => {
                                for d in exprs::describe(&items) {
                                    println!("{}", dim(&d));
                                }
                                println!("{}", serde_json::to_string_pretty(&spec.to_value())?);
                            }
                            Err(e) => println!("error: {e}"),
                        }
                    }
                    "" | "json" => {
                        // A JSON spec (`.json {...}` or a bare object), else the shorthand line.
                        let json_text = if cmd == "json" { Some(rest) } else if line.starts_with('{') { Some(line) } else { None };
                        let t0 = Instant::now();
                        let parsed: Result<zsql_spec::Spec, String> = match json_text {
                            Some(text) => zsql_spec::Spec::from_json(text).map_err(|e| e.to_string()),
                            None => exprs::parse_line(line).map(|(spec, items)| {
                                for d in exprs::describe(&items) {
                                    println!("{}", dim(&d));
                                }
                                spec
                            }),
                        };
                        let lowered = parsed.and_then(|spec| zsql_spec::lower(&spec, &universe, &base).map_err(|e| e.to_string()));
                        let t1 = Instant::now();
                        let planned = lowered.and_then(|q| planner.plan_query(q, context.as_ref()).map_err(|e| format!("{}: {e}", e.rails_class())));
                        let t2 = Instant::now();
                        match planned {
                            Ok(plan) => {
                                println!("{}", bold(&plan.sql));
                                println!(
                                    "{}",
                                    dim(&format!(
                                        "-- {} · {:.2} ms (resolve {:.2} ms, plan {:.2} ms)",
                                        plan.adapter,
                                        (t2 - t0).as_secs_f64() * 1e3,
                                        (t1 - t0).as_secs_f64() * 1e3,
                                        (t2 - t1).as_secs_f64() * 1e3
                                    ))
                                );
                            }
                            Err(e) => println!("error: {e}"),
                        }
                    }
                    other => println!("unknown command .{other}; .help lists them"),
                }
            }
            if let (Some(ed), Some(h)) = (editor.as_mut(), &history) {
                let _ = ed.save_history(h);
            }
        }
        Command::Golden { filter, strict, fixtures, dump, paths } => {
            let source = if paths == "snapshot" { PathSource::Snapshot } else { PathSource::Formed };
            let fx = Fixtures::open(fixtures.unwrap_or_else(Fixtures::default_root));
            let report = score(&fx, &filter, source)?;
            let (total, passed) = print_scoreboard(&report);
            if let Some(dir) = dump {
                std::fs::create_dir_all(&dir)?;
                let cases = fx.cases()?;
                for fs in report.values() {
                    for (id, outcome) in &fs.failed {
                        let case = cases.values().flatten().find(|c| &c.id == id).unwrap();
                        let safe = id.replace(['#', '/', ' '], "_");
                        let expected = match &case.expected {
                            zsql_golden::Expected::Sql { sql } => sql.clone(),
                            zsql_golden::Expected::Error { error } => format!("ERROR {}: {}", error.class, error.message),
                        };
                        let got = match outcome {
                            zsql_golden::Outcome::SqlMismatch { got } | zsql_golden::Outcome::NoError { got } => got.clone(),
                            zsql_golden::Outcome::WrongError { got } => format!("ERROR {got}"),
                            other => format!("{other:?}"),
                        };
                        std::fs::write(dir.join(format!("{safe}.expected.sql")), expected)?;
                        std::fs::write(dir.join(format!("{safe}.got.sql")), got)?;
                        std::fs::write(dir.join(format!("{safe}.spec.json")), serde_json::to_string_pretty(&case.spec)?)?;
                    }
                }
            }
            if strict && passed != total {
                bail!("{} of {} golden cases failing", total - passed, total);
            }
        }
        Command::Bench { fixtures, rounds } => {
            use std::time::Duration;
            let fx = Fixtures::open(fixtures.unwrap_or_else(Fixtures::default_root));
            let dialects = fx.dialects();
            let base = dialects.base()?;
            let cases: Vec<_> = fx.cases()?.into_values().flatten().collect();
            let hashes = fx.snapshot_hashes()?;

            let t = Instant::now();
            for h in &hashes {
                fx.universe(h)?;
            }
            let load = t.elapsed();
            let t = Instant::now();
            for h in &hashes {
                fx.formed_paths(h)?;
            }
            let form = t.elapsed();
            let held: Vec<(String, std::sync::Arc<Universe>, std::sync::Arc<Vec<zsql_semantic::Path>>)> =
                hashes.iter().map(|h| Ok((h.clone(), fx.universe(h)?, fx.formed_paths(h)?))).collect::<Result<_>>()?;
            let t = Instant::now();
            let planners: std::collections::HashMap<&str, zsql_planner::Planner<'_>> =
                held.iter().map(|(h, u, p)| (h.as_str(), zsql_planner::Planner::new(u, dialects, Some(p)))).collect();
            let index = t.elapsed();

            let (mut lower_time, mut plan_time, mut compare_time) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
            let mut n = 0usize;
            for _ in 0..rounds {
                for case in &cases {
                    let universe = fx.universe(&case.universe)?;
                    let planner = &planners[case.universe.as_str()];
                    let t = Instant::now();
                    let lowered = zsql_spec::Spec::from_value(case.spec.clone()).and_then(|s| zsql_spec::lower(&s, &universe, &base));
                    lower_time += t.elapsed();
                    if let Ok(q) = lowered {
                        let t = Instant::now();
                        let planned = planner.plan_query(q, case.context.as_ref());
                        plan_time += t.elapsed();
                        n += 1;
                        if let (Ok(p), zsql_golden::Expected::Sql { sql }) = (&planned, &case.expected) {
                            let t = Instant::now();
                            let _ = zsql_golden::structural_form(&p.sql) == zsql_golden::structural_form(sql);
                            compare_time += t.elapsed();
                        }
                    }
                }
            }
            let per = |d: Duration, n: usize| if n == 0 { 0.0 } else { d.as_secs_f64() * 1e6 / n as f64 };
            println!("{:<28} {:>10} {:>12}", "phase", "total ms", "per item us");
            println!("{:<28} {:>10.1} {:>12.1}", format!("load {} universes", hashes.len()), load.as_secs_f64() * 1e3, per(load, hashes.len()));
            println!("{:<28} {:>10.1} {:>12.1}", format!("form {} universes", hashes.len()), form.as_secs_f64() * 1e3, per(form, hashes.len()));
            println!("{:<28} {:>10.1} {:>12.1}", format!("index {} universes", hashes.len()), index.as_secs_f64() * 1e3, per(index, hashes.len()));
            println!("{:<28} {:>10.1} {:>12.1}", format!("lower spec x{n}"), lower_time.as_secs_f64() * 1e3, per(lower_time, n));
            println!("{:<28} {:>10.1} {:>12.1}", format!("plan x{n}"), plan_time.as_secs_f64() * 1e3, per(plan_time, n));
            println!("{:<28} {:>10.1} {:>12.1}", "structural compare", compare_time.as_secs_f64() * 1e3, per(compare_time, n));
            let phases = zsql_planner::profile::report();
            if !phases.is_empty() {
                println!("\n{:<28} {:>10} {:>12} {:>8}", "planner phase", "total ms", "per call us", "calls");
                for (name, d, calls) in phases {
                    println!("{:<28} {:>10.1} {:>12.1} {:>8}", name, d.as_secs_f64() * 1e3, per(d, calls as usize), calls);
                }
            } else {
                println!("\n(set ZSQL_PROFILE=1 for per-phase planner timings)");
            }
        }
        Command::Values { command } => values_command(command)?,
        Command::Deploy { project, server, uid, branch } => {
            let uid = remote::project_uid(uid, Some(&project))?;
            let archive = project_archive(&project)?;
            let client = remote::Client::new(&server);
            println!("deploying {} ({} bytes) to {}/projects/{uid}/branches/{branch}", project.display(), archive.len(), client.server);
            let v = client.post_bytes(&format!("/projects/{uid}/branches/{branch}/deploy"), "application/gzip", &archive)?;
            remote::print_json(&v)?;
        }
        Command::Remote { command } => remote_command(command)?,
        Command::Form { source } => {
            let from_project = source.project.is_some();
            let model = source.load()?;
            let stored = model.snapshot.paths.clone();
            let universe = Universe::build(model.snapshot)?;
            let formed = zsql_formation::form(&universe);
            if from_project {
                println!("{} paths over {} root tables", formed.paths.len(), universe.tables().len());
            } else {
                let report = zsql_golden::compare_paths(&universe, &stored, &formed.paths);
                println!("{}", report.summary());
                for line in report.mismatches.iter().take(20) {
                    println!("  {line}");
                }
            }
            println!("{} warnings", formed.warnings.len() + model.warnings.len());
            for w in model.warnings.iter().chain(formed.warnings.iter()) {
                println!("warning: {w}");
            }
        }
    }
    Ok(())
}

fn print_values(d: &zsql_project::values::DimensionValues, brief: bool) {
    let when = d.sampled_at.get(..10).unwrap_or(&d.sampled_at);
    match (&d.range, d.values.is_empty()) {
        (Some(r), _) => println!("{:<40} {} .. {}  (distinct {}, {})", d.uid, r.min, r.max, d.distinct.map(|n| n.to_string()).unwrap_or_else(|| "?".into()), when),
        (None, false) => {
            println!("{:<40} {} of {} values, {:?}, {}", d.uid, d.values.len(), d.distinct.map(|n| n.to_string()).unwrap_or_else(|| "?".into()), d.mode, when);
            let shown = if brief { 5 } else { d.values.len() };
            for v in d.values.iter().take(shown) {
                println!("    {:<60} {}", v.v, v.n);
            }
            if shown < d.values.len() {
                println!("    ... {} more", d.values.len() - shown);
            }
        }
        (None, true) => println!("{:<40} no values ({})", d.uid, when),
    }
}

fn values_command(command: ValuesCommand) -> Result<()> {
    use zsql_project::values::{DimensionValues, Range, ValueCatalog, ValueCount, FILE_NAME, FULL_CEILING};
    match command {
        ValuesCommand::Show { project, dim } => {
            let catalog = ValueCatalog::read(&project.join(FILE_NAME))?;
            let Some(catalog) = catalog else {
                println!("no {FILE_NAME} in {}", project.display());
                return Ok(());
            };
            let needle = dim.unwrap_or_default().to_lowercase();
            for d in catalog.dimensions.iter().filter(|d| needle.is_empty() || d.uid.to_lowercase().contains(&needle) || d.name.to_lowercase().contains(&needle)) {
                print_values(d, needle.is_empty());
            }
        }
        ValuesCommand::Sample { project, dims, full, older_than, dry_run } => {
            let dialects = Dialects::builtin();
            let loaded = zsql_project::load(&project, &dialects)?;
            for w in &loaded.warnings {
                eprintln!("warning: {w}");
            }
            let universe = Universe::build(loaded.snapshot)?;
            let mut catalog = loaded.values.unwrap_or_default();
            let qualify = |sql: &str, columns: &[String], adapter: &str, alias: &str| -> String {
                match dialects.get(adapter) {
                    Ok(d) => zsql_planner::sql::expr::qualify(sql, columns, &d, alias),
                    Err(_) => sql.to_string(),
                }
            };
            let (jobs, skipped) = zsql_project::values::plan(&universe, &dims, full, &qualify);
            for (uid, why) in &skipped {
                if !dims.is_empty() {
                    println!("skip {uid}: {why}");
                }
            }
            let now = chrono::Utc::now();
            let stale_before = older_than.map(|days| now - chrono::Duration::days(days));
            let mut todo: Vec<_> = jobs
                .into_iter()
                .filter(|j| match (stale_before, catalog.get(&j.uid)) {
                    (Some(cut), Some(existing)) => chrono::DateTime::parse_from_rfc3339(&existing.sampled_at).map(|t| t < cut).unwrap_or(true),
                    _ => true,
                })
                .collect();
            todo.sort_by(|a, b| (a.datasource_uid.clone(), a.uid.clone()).cmp(&(b.datasource_uid.clone(), b.uid.clone())));
            if dry_run {
                for j in &todo {
                    println!("-- {} ({}) on {} via {}\n{}\n{}\n", j.uid, j.name, j.table_uid, j.datasource_uid, j.distinct_sql, j.values_sql.clone().or(j.range_sql.clone()).unwrap_or_default());
                }
                println!("{} dimensions to sample, {} skipped", todo.len(), skipped.len());
                return Ok(());
            }
            let mut connections: std::collections::HashMap<String, Box<dyn zsql_warehouse::Warehouse>> = Default::default();
            let (mut done, mut failed) = (0, 0);
            for j in &todo {
                if !connections.contains_key(&j.datasource_uid) {
                    let settings = zsql_warehouse::settings_for(&project, &j.datasource_uid)?;
                    match zsql_warehouse::connect(&settings) {
                        Ok(c) => {
                            connections.insert(j.datasource_uid.clone(), c);
                        }
                        Err(e) => bail!("datasource {}: {e}", j.datasource_uid),
                    }
                }
                let wh = connections.get_mut(&j.datasource_uid).unwrap();
                let outcome = (|| -> Result<DimensionValues> {
                    let distinct = wh.query(&j.distinct_sql)?.first().and_then(|r| r.first().cloned()).flatten().and_then(|n| n.parse::<i64>().ok());
                    let mut entry = DimensionValues {
                        uid: j.uid.clone(),
                        name: j.name.clone(),
                        table: j.table_uid.clone(),
                        sampled_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        mode: j.mode,
                        distinct,
                        values: vec![],
                        range: None,
                    };
                    if let Some(sql) = &j.values_sql {
                        if j.cap.is_none() && distinct.map(|n| n > FULL_CEILING).unwrap_or(false) {
                            bail!("{} has {} distinct values; a full sample is refused above {FULL_CEILING}. Use the default sample or raise value_list_size.", j.uid, distinct.unwrap());
                        }
                        entry.values = wh
                            .query(sql)?
                            .into_iter()
                            .filter_map(|r| Some(ValueCount { v: r.first().cloned().flatten()?, n: r.get(1).cloned().flatten().and_then(|n| n.parse().ok()).unwrap_or(0) }))
                            .collect();
                    }
                    if let Some(sql) = &j.range_sql {
                        let row = wh.query(sql)?.into_iter().next().unwrap_or_default();
                        if let (Some(Some(lo)), Some(Some(hi))) = (row.first().cloned(), row.get(1).cloned()) {
                            entry.range = Some(Range { min: lo, max: hi });
                        }
                    }
                    Ok(entry)
                })();
                match outcome {
                    Ok(entry) => {
                        let what = match (&entry.range, entry.values.len()) {
                            (Some(r), _) => format!("{} .. {}", r.min, r.max),
                            (None, n) => format!("{n} values"),
                        };
                        println!("{:<40} {what}", j.uid);
                        catalog.put(entry);
                        done += 1;
                    }
                    Err(e) => {
                        failed += 1;
                        eprintln!("{:<40} FAILED: {e}", j.uid);
                    }
                }
            }
            catalog.write(&project.join(FILE_NAME))?;
            println!("{done} sampled, {failed} failed, {} skipped; wrote {}", skipped.len(), project.join(FILE_NAME).display());
            if failed > 0 {
                bail!("{failed} dimension(s) failed to sample");
            }
        }
    }
    Ok(())
}

/// The project as a tar.gz: YAML models, tests, security, datasources.yml
/// with its secrets removed, and values.jsonl.gz. Never .strata, .git or
/// data files.
fn project_archive(project: &std::path::Path) -> Result<Vec<u8>> {
    const SECRET_KEYS: [&str; 7] = ["password", "private_key", "personal_access_token", "secret_access_key", "access_key_id", "oauth_client_secret", "oauth_client_id"];
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
            let mut v: serde_yaml_value::Value = serde_yaml_value::from_str(&text)?;
            if let Some(map) = v.as_mapping_mut() {
                for (_, ds) in map.iter_mut() {
                    if let Some(entry) = ds.as_mapping_mut() {
                        for k in SECRET_KEYS {
                            entry.remove(serde_yaml_value::Value::String(k.into()));
                        }
                    }
                }
            }
            add("datasources.yml", serde_yaml_value::to_string(&v)?.as_bytes())?;
        }
        if let Ok(bytes) = std::fs::read(project.join(zsql_project::values::FILE_NAME)) {
            add(zsql_project::values::FILE_NAME, &bytes)?;
        }
        for dir in ["models", "tests", "migrations"] {
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

fn walk_yaml(dir: &std::path::Path, out: &mut Vec<PathBuf>) -> Result<()> {
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

fn remote_command(command: RemoteCommand) -> Result<()> {
    let target_of = |t: &Target| -> Result<(remote::Client, String, String)> {
        Ok((remote::Client::new(&t.server), remote::project_uid(t.uid.clone(), t.project.as_deref())?, t.branch.clone()))
    };
    match command {
        RemoteCommand::Health { server } => {
            let v = remote::Client::new(&server).get("/healthz")?;
            println!("{}", v.as_str().unwrap_or("ok"));
        }
        RemoteCommand::List { server } => {
            let v = remote::Client::new(&server).get("/projects")?;
            for d in v["deployments"].as_array().into_iter().flatten() {
                println!("{:<32} {:<12} {} tables, {} fields, {} policies, {} values", d["project"].as_str().unwrap_or(""), d["branch"].as_str().unwrap_or(""), d["tables"], d["fields"], d["policies"], d["values"]);
            }
        }
        RemoteCommand::Show { target } => {
            let (client, uid, branch) = target_of(&target)?;
            remote::print_json(&client.get(&format!("/projects/{uid}/branches/{branch}"))?)?;
        }
        RemoteCommand::Plan { target, spec, expr, context, json } => {
            let (client, uid, branch) = target_of(&target)?;
            let spec_value = match (spec, expr) {
                (Some(path), _) => {
                    let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
                    let value: serde_json::Value = serde_json::from_str(&text)?;
                    value.get("spec").cloned().unwrap_or(value)
                }
                (None, Some(line)) => {
                    let (spec, items) = exprs::parse_line(&line).map_err(|e| anyhow::anyhow!(e))?;
                    for d in exprs::describe(&items) {
                        eprintln!("{d}");
                    }
                    spec.to_value()
                }
                (None, None) => bail!("remote plan needs --spec or --expr"),
            };
            let context: Option<serde_json::Value> = match context {
                Some(p) => Some(serde_json::from_str(&std::fs::read_to_string(&p).with_context(|| p.display().to_string())?)?),
                None => None,
            };
            let v = remote::plan(&client, &uid, &branch, spec_value, context.as_ref(), json)?;
            if json {
                remote::print_json(&v)?;
            } else {
                println!("{}", v["sql"].as_str().unwrap_or(""));
            }
        }
        RemoteCommand::Fields { target, q, hidden } => {
            let (client, uid, branch) = target_of(&target)?;
            let q = q.unwrap_or_default();
            remote::print_fields(&client.get(&format!("/projects/{uid}/branches/{branch}/fields?q={}&hidden={hidden}", q))?);
        }
        RemoteCommand::Tables { target } => {
            let (client, uid, branch) = target_of(&target)?;
            remote::print_tables(&client.get(&format!("/projects/{uid}/branches/{branch}/tables"))?);
        }
        RemoteCommand::Values { target, dim } => {
            let (client, uid, branch) = target_of(&target)?;
            remote::print_values(&remote::values_for(&client, &uid, &branch, &dim)?);
        }
        RemoteCommand::Repl { target, context } => {
            let (client, uid, branch) = target_of(&target)?;
            let context: Option<serde_json::Value> = match context {
                Some(p) => Some(serde_json::from_str(&std::fs::read_to_string(&p).with_context(|| p.display().to_string())?)?),
                None => None,
            };
            remote::repl(&client, &uid, &branch, context)?;
        }
    }
    Ok(())
}

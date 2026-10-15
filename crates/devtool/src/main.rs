//! zsql-dev: the engine on the command line, for its developers. What
//! a user does with a model is `zsql`'s job (it plans here or on a
//! server); this is for checking the engine itself: the golden
//! scoreboard, the benches, formation against stored paths, and
//! snapshot export.

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;
use zsql_dialect::Dialects;
use zsql_golden::{print_scoreboard, score, Fixtures, PathSource};
use zsql_semantic::{Snapshot, Universe};

#[derive(Parser)]
#[command(name = "zsql-dev", about = "zsql-dev: the engine on the command line, for its developers")]
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
}

impl Source {
    fn load(&self) -> Result<Model> {
        let dialects = match &self.dialects {
            Some(dir) => Dialects::new(dir),
            None => Dialects::builtin(),
        };
        match (&self.snapshot, &self.project) {
            (Some(path), _) => {
                Ok(Model { snapshot: Snapshot::from_path(path).with_context(|| path.display().to_string())?, warnings: vec![] })
            }
            (None, Some(dir)) => {
                let loaded = zsql_project::load(dir, &dialects)?;
                Ok(Model { snapshot: loaded.snapshot, warnings: loaded.warnings })
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

/// Where the goldens take their join paths from.
#[derive(Clone, Copy, ValueEnum)]
enum Paths {
    Formed,
    Snapshot,
}

#[derive(Subcommand)]
enum Command {
    /// Load the model, build the universe and report what is in it.
    Check {
        #[command(flatten)]
        source: Source,
    },
    /// Write the loaded model as a snapshot file (`.json`, or `.json.gz` when the name ends in .gz).
    Export {
        #[command(flatten)]
        source: Source,
        #[arg(long)]
        out: PathBuf,
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
        /// Join paths to plan over: formed here, or the ones stored with the snapshot.
        #[arg(long, value_enum, default_value_t = Paths::Formed)]
        paths: Paths,
    },
    /// Time loading a project directory, step by step: the YAML load with formation, the universe index, the planner index.
    LoadBench {
        #[arg(long)]
        project: PathBuf,
        #[arg(long, default_value_t = 20)]
        rounds: usize,
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
        Command::Golden { filter, strict, fixtures, dump, paths } => {
            let source = match paths {
                Paths::Formed => PathSource::Formed,
                Paths::Snapshot => PathSource::Snapshot,
            };
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
        Command::LoadBench { project, rounds } => {
            use std::collections::BTreeMap;
            use std::time::Duration;
            let dialects = Dialects::builtin();
            let (mut load, mut index, mut plan) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
            let mut steps: BTreeMap<&'static str, (usize, u128)> = BTreeMap::new();
            let mut order: Vec<&'static str> = Vec::new();
            for _ in 0..rounds.max(1) {
                let t = Instant::now();
                let loaded = zsql_project::load(&project, &dialects)?;
                load += t.elapsed();
                for (i, (name, us)) in loaded.phases.iter().enumerate() {
                    let e = steps.entry(name).or_insert((i, 0));
                    e.1 += us;
                    if !order.contains(name) {
                        order.push(name);
                    }
                }
                let t = Instant::now();
                let universe = Universe::build(loaded.snapshot)?;
                index += t.elapsed();
                let t = Instant::now();
                let _p = zsql_planner::Planner::new(&universe, &dialects, None);
                plan += t.elapsed();
            }
            let n = rounds.max(1) as f64;
            println!("{:<20} {:>10}", "step", "per load us");
            for name in order {
                println!("  {:<18} {:>10.0}", name, steps[name].1 as f64 / n);
            }
            println!("{:<20} {:>10.0}", "load (yaml+form)", load.as_micros() as f64 / n);
            println!("{:<20} {:>10.0}", "universe index", index.as_micros() as f64 / n);
            println!("{:<20} {:>10.0}", "planner index", plan.as_micros() as f64 / n);
            println!("{:<20} {:>10.0}", "total", (load + index + plan).as_micros() as f64 / n);
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
            println!(
                "{:<28} {:>10.1} {:>12.1}",
                format!("load {} universes", hashes.len()),
                load.as_secs_f64() * 1e3,
                per(load, hashes.len())
            );
            println!(
                "{:<28} {:>10.1} {:>12.1}",
                format!("form {} universes", hashes.len()),
                form.as_secs_f64() * 1e3,
                per(form, hashes.len())
            );
            println!(
                "{:<28} {:>10.1} {:>12.1}",
                format!("index {} universes", hashes.len()),
                index.as_secs_f64() * 1e3,
                per(index, hashes.len())
            );
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

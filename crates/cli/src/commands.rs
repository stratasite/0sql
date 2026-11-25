//! The commands: one file each, and the one function that sends a parsed
//! command line to the right one.

pub(crate) mod catalog;
pub(crate) mod catalog_check;
pub(crate) mod datasource;
pub(crate) mod deploy;
pub(crate) mod generate;
pub(crate) mod repl;
pub(crate) mod run;
pub(crate) mod sample;
pub(crate) mod scaffold;
#[cfg(feature = "server")]
pub(crate) mod serve;

use crate::cli::{read_json, CatalogWhat, Cli, Command, Mode, ResultsWhat, Where};
use crate::side::Session;
use crate::{failure, remote, show, tty};
use anyhow::{bail, Result};
use std::path::PathBuf;
use std::time::Instant;

pub(crate) fn run(command: Command, project: Option<PathBuf>) -> Result<()> {
    let here = Where::here(project.clone());
    let dir = project.as_deref();
    match command {
        Command::Spec { line } => {
            let (spec, _, items) =
                zsql_core::spec_of(None, Some(&line.join(" ")), &zsql_core::unlimited).map_err(failure::Refusal::from)?;
            for d in items {
                eprintln!("{d}");
            }
            println!("{}", serde_json::to_string_pretty(&spec)?);
            Ok(())
        }
        Command::Init { dir, name, uid, no_git } => scaffold::init(dir, name, uid, no_git),
        Command::Datasource { what } => datasource::run(dir, what),
        Command::Deploy { dry_run, watch, skip_check, clear_secrets, remote } => {
            deploy::deploy(Where::with(project, remote), deploy::How { dry_run, watch, skip_check, clear_secrets })
        }
        Command::Run { query, limit, all, fresh, timeout, format, out, head, quiet } => {
            let format = format.or_else(|| tty::agent().then(|| "json".to_string()));
            let (mode, q, context) = query.resolve(here)?;
            let Mode::Local(local) = mode else { unreachable!("no remote flags on run") };
            let model = local.model()?;
            run::run(
                &local.project,
                &model,
                &q,
                context.as_ref(),
                run::Options { limit, all, fresh, timeout, format: format.as_deref(), out: out.as_deref(), head, quiet },
            )
        }
        Command::Results { what } => {
            let project = here.project_dir()?;
            let cmd = match &what {
                None => run::ResultsCommand::List { json: tty::agent() },
                Some(ResultsWhat::List { json }) => run::ResultsCommand::List { json: *json || tty::agent() },
                Some(ResultsWhat::Show { id, format, head, offset }) => {
                    run::ResultsCommand::Show { id, format: format.as_deref(), head: *head, offset: *offset }
                }
                Some(ResultsWhat::Export { id, out }) => run::ResultsCommand::Export { id, out },
                Some(ResultsWhat::Sql { id }) => run::ResultsCommand::Sql { id },
                Some(ResultsWhat::Rm { ids }) => run::ResultsCommand::Remove { ids },
                Some(ResultsWhat::Purge { yes }) => run::ResultsCommand::Purge { yes: *yes },
            };
            run::results(&project, cmd)
        }
        Command::Catalog { what, datasource, json } => match what {
            None => catalog::status(dir, datasource.as_deref(), json || tty::agent()),
            Some(CatalogWhat::Read { tables, schema, all, datasource, json: want }) => {
                catalog::read(dir, datasource.as_deref(), &schema, &tables, all, want || json || tty::agent())
            }
            Some(CatalogWhat::Add { tables, datasource }) => {
                catalog::read(dir, datasource.as_deref(), &[], &tables, false, json || tty::agent())
            }
            Some(CatalogWhat::Sync { datasource, json: want }) => catalog::sync(dir, datasource.as_deref(), want || json || tty::agent()),
            Some(CatalogWhat::Search { pattern, live, json, datasource }) => {
                catalog::search(dir, datasource.as_deref(), &pattern, live, json || tty::agent())
            }
            Some(CatalogWhat::Show { table, json, datasource }) => catalog::show(dir, datasource.as_deref(), &table, json || tty::agent()),
        },
        Command::Sample { names, datasource, dry_run, budget, yes, json } => sample::run(
            dir,
            sample::Options { datasource: datasource.as_deref(), names: &names, dry_run, budget, yes, json: json || tty::agent() },
        ),
        Command::Generate { relationships: Some(path), infer, datasource, dry_run, .. } => generate::relationships(
            dir,
            generate::RelOptions { path: Some(path.as_str()), infer, datasource: datasource.as_deref(), dry_run },
        ),
        Command::Generate {
            tables,
            datasource,
            schema,
            all,
            columns,
            new_columns,
            live,
            empty: true,
            physical_name,
            domain,
            dir: into,
            dry_run,
            ..
        } => {
            let _ = (schema, all, columns, new_columns, live);
            let Some(named) = tables.first() else { bail!("--empty needs the table's name: zsql generate orders --empty") };
            // `help/sessions` says the directory and the name at once, which
            // is how the rest of generate takes a target directory; --dir and
            // --domain say the same thing the long way. A trailing slash is a
            // directory everywhere else, so here it is a missing table name
            // rather than a table whose name ends in one.
            let (from_path, name) = match named.rsplit_once('/') {
                Some((_, "")) => bail!("{named} names a directory, not a table: zsql generate {}orders --empty", named),
                Some((d, n)) => (Some(d.trim_matches('/').to_string()), n),
                None => (None, named.as_str()),
            };
            let domain = domain.or(into).or(from_path);
            // An empty scaffold needs no warehouse, so a project with no
            // datasource yet still gets its file, with the key left blank
            // for whatever is added later.
            let project_dir = here.project_dir()?.dir;
            let uid = if datasource.is_none() && !catalog::any_datasource(&project_dir)? {
                String::new()
            } else {
                catalog::choose(&project_dir, datasource.as_deref())?.uid
            };
            scaffold::empty_table(dir, name, &uid, physical_name.as_deref(), domain.as_deref(), dry_run)
        }
        Command::Generate { tables, datasource, schema, all, columns, new_columns, live, overwrite, dry_run, dir: into, .. } => {
            // `generate coupons/ orders` names the directory and a table.
            let (slashed, tables): (Vec<String>, Vec<String>) = tables.into_iter().partition(|t| t.ends_with('/'));
            let into = into.or_else(|| slashed.first().map(|d| d.trim_end_matches('/').to_string()));
            if slashed.len() > 1 {
                bail!("one directory at a time: {}", slashed.join(", "));
            }
            generate::run(
                dir,
                generate::Options {
                    datasource: datasource.as_deref(),
                    tables: &tables,
                    dir: into.as_deref(),
                    schema: schema.as_deref(),
                    all,
                    columns: &columns,
                    new_columns,
                    live,
                    overwrite,
                    dry_run,
                },
            )
        }
        Command::Check { json, remote } => {
            let at = Where::with(project, remote);
            let t0 = Instant::now();
            let mut side = at.mode()?.side()?;
            let v = side.check()?;
            if json || tty::agent() {
                // One document: the whole check, and the failure in it.
                let ok = show::deploy_ok(&v);
                let mut v = v;
                if !ok {
                    v["error"] = serde_json::json!({ "class": "Error", "message": "tests failed" });
                }
                show::print_json(&v)?;
                return if ok { Ok(()) } else { Err(failure::Reported("tests failed".into()).into()) };
            }
            let ok = show::print_deploy(&v);
            if side.origin() == "local" {
                eprintln!("-- local {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
                show::next_after_check(&v, &at.project_dir()?);
            }
            if !ok {
                bail!("tests failed");
            }
            Ok(())
        }
        Command::Test { new: Some(name), json, .. } => scaffold::new_test(dir, &name, json || tty::agent()),
        Command::Test { json, remote, .. } => {
            let v = Where::with(project, remote).mode()?.side()?.test()?;
            if json || tty::agent() {
                let ok = show::tests_passed(&v);
                let mut v = v;
                if !ok {
                    v["error"] = serde_json::json!({ "class": "Error", "message": "tests failed" });
                }
                show::print_json(&v)?;
                return if ok { Ok(()) } else { Err(failure::Reported("tests failed".into()).into()) };
            }
            if !show::print_tests(&v) {
                bail!("tests failed");
            }
            Ok(())
        }
        Command::Sql { query, explain, json, remote } => query.run(Where::with(project, remote), explain, json || tty::agent()),
        Command::Explain { query, json, remote } => query.run(Where::with(project, remote), true, json || tty::agent()),
        Command::Explore { query, q, json, remote } => {
            let (mode, query, context) = query.resolve(Where::with(project, remote))?;
            let mut side = mode.side()?;
            let v = side.explore(&query, context.as_ref(), q.as_deref())?;
            if json || tty::agent() {
                show::print_json(&v)?;
            } else {
                show::print_explore(&v, &|s| s.to_string(), side.origin());
            }
            Ok(())
        }
        Command::Repl { context, remote } => {
            let context = read_json(context)?;
            match Where::with(project, remote).mode()? {
                Mode::Local(l) => repl::run(Session::open(l)?, context),
                Mode::Remote(t) => repl::run(t, context),
            }
        }
        Command::Fields { term, hidden, json, remote } => {
            let v = Where::with(project, remote).mode()?.side()?.fields(&term.unwrap_or_default(), hidden)?;
            if json || tty::agent() {
                show::print_json(&v)
            } else {
                show::print_fields(&v);
                Ok(())
            }
        }
        Command::Tables { json, remote } => {
            let v = Where::with(project, remote).mode()?.side()?.tables()?;
            if json || tty::agent() {
                show::print_json(&v)
            } else {
                show::print_tables(&v);
                Ok(())
            }
        }
        Command::Values { dim, json, remote } => {
            let v = Where::with(project, remote).mode()?.side()?.values(&dim)?;
            if json || tty::agent() {
                show::print_json(&v)
            } else {
                show::print_values(&v);
                Ok(())
            }
        }
        Command::Status { remote } => {
            let t = Where::with(project, remote).target(false)?;
            show::print_json(&t.client.get(&remote::branch_path(t.org.as_deref(), &t.uid, &t.branch))?)
        }
        Command::Deployments { remote } => {
            let at = Where::with(project, remote);
            let v = at.client().get(&format!("{}/projects", remote::org_path(at.org().as_deref())))?;
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
        Command::Undeploy { yes, remote } => {
            let t = Where::with(project, remote).target(false)?;
            let sure = yes || tty::confirm(&format!("remove {} from {}?", t.name(), t.client.server), "--yes")?;
            if !sure {
                bail!("nothing removed");
            }
            show::print_json(&t.client.delete(&remote::branch_path(t.org.as_deref(), &t.uid, &t.branch))?)
        }
        Command::Health { remote } => {
            let client = Where::with(project, remote).client();
            let v = client.get("/healthz")?;
            println!("{} {}", client.server, v.as_str().unwrap_or("ok"));
            Ok(())
        }
        Command::Completions { shell } => {
            use clap::CommandFactory;
            clap_complete::generate(shell, &mut Cli::command(), "zsql", &mut std::io::stdout());
            Ok(())
        }
        #[cfg(feature = "server")]
        Command::Serve(args) => serve::serve(args, here),
        #[cfg(feature = "server")]
        Command::Server { what } => serve::server(what, here),
    }
}

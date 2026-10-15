//! The interactive loop, over one of two sides: the project directory
//! at hand, held in memory, or a server's copy of the model.
//!
//! The local side loads the model once and keeps it. Before every line
//! it stats the files the load read, imports included, and when any of
//! them moved it loads again and says so; a load that fails keeps the
//! model from before and says that instead. `.reload` forces one.
//! The remote side is the server as before, and `.branch` switches
//! which of its branches is asked.

use crate::failure::Refusal;
use crate::local::{self, Local};
use crate::{config, remote, show, Target};
use anyhow::Result;
use serde_json::Value;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Instant, SystemTime};
use zsql_service::Model;

/// What the loop asks of whichever side it is on. The reads take
/// `&mut self` because the local side may reload underneath them.
pub trait Side {
    /// `server` or `local`: who planned it, for the footer.
    fn origin(&self) -> &'static str;
    /// Prints the summary line; answers with every field's uid and
    /// name, lower-cased, so a bare `help` can tell a field from a word.
    fn announce(&mut self) -> Result<Vec<String>>;
    /// Called before each line. True when the model was reloaded and
    /// the field names should be read again.
    fn refresh(&mut self) -> Result<bool>;
    /// `.reload`: load again whether or not anything moved.
    fn reload(&mut self) -> Result<()>;
    /// `.branch`: show the branch, or switch to one; true when switched.
    fn branch(&mut self, to: Option<&str>) -> Result<bool>;
    fn tables(&mut self) -> Result<Value>;
    fn fields(&mut self, q: &str) -> Result<Value>;
    fn values(&mut self, dim: &str) -> Result<Value>;
    fn explore(&mut self, spec: Option<&Value>, expr: Option<&str>, context: Option<&Value>, term: Option<&str>) -> Result<Value>;
    fn sql(&mut self, spec: Option<&Value>, expr: Option<&str>, context: Option<&Value>, explain: bool) -> Result<Value>;
}

// ------------------------------------------------------------------ remote

impl Side for Target {
    fn origin(&self) -> &'static str {
        "server"
    }

    fn announce(&mut self) -> Result<Vec<String>> {
        let summary = self.client.get(&remote::branch_path(&self.uid, &self.branch))?;
        println!("{}", dim(&format!("{}/{} on {}: {} tables, {} fields, {} joins, {} policies", self.uid, self.branch, self.client.server, summary["tables"], summary["fields"], summary["joins"], summary["policies"])));
        Ok(names_in(&self.client.get(&format!("{}/fields?hidden=true", remote::branch_path(&self.uid, &self.branch)))?))
    }

    fn refresh(&mut self) -> Result<bool> {
        Ok(false)
    }

    fn reload(&mut self) -> Result<()> {
        println!("{}", dim("the server holds the model; `zsql deploy` sends it a new one"));
        Ok(())
    }

    fn branch(&mut self, to: Option<&str>) -> Result<bool> {
        match to {
            None => {
                println!("{}", self.branch);
                Ok(false)
            }
            Some(b) => {
                self.branch = b.to_string();
                Ok(true)
            }
        }
    }

    fn tables(&mut self) -> Result<Value> {
        self.client.get(&format!("{}/tables", remote::branch_path(&self.uid, &self.branch)))
    }

    fn fields(&mut self, q: &str) -> Result<Value> {
        self.client.get(&format!("{}/fields?q={}", remote::branch_path(&self.uid, &self.branch), remote::urlencode(q)))
    }

    fn values(&mut self, dim: &str) -> Result<Value> {
        remote::values_for(&self.client, &self.uid, &self.branch, dim)
    }

    fn explore(&mut self, spec: Option<&Value>, expr: Option<&str>, context: Option<&Value>, term: Option<&str>) -> Result<Value> {
        remote::explore(&self.client, &self.uid, &self.branch, spec, expr, context, term)
    }

    fn sql(&mut self, spec: Option<&Value>, expr: Option<&str>, context: Option<&Value>, explain: bool) -> Result<Value> {
        remote::sql(&self.client, &self.uid, &self.branch, spec, expr, context, explain)
    }
}

// ------------------------------------------------------------------- local

/// A file as last seen: its size and modification time, or that it
/// was not there.
type Stamp = (PathBuf, Option<(u64, SystemTime)>);

fn stamps(sources: &[PathBuf]) -> Vec<Stamp> {
    sources.iter().map(|p| (p.clone(), std::fs::metadata(p).ok().and_then(|m| Some((m.len(), m.modified().ok()?))))).collect()
}

/// The project directory, loaded and held.
pub struct Session {
    local: Local,
    model: Model,
    /// The sources as they were when the model was loaded, or when a
    /// reload last failed, so a failure is reported once and not on
    /// every line until the file is fixed.
    seen: Vec<Stamp>,
}

impl Session {
    pub fn open(local: Local) -> Result<Session> {
        let t0 = Instant::now();
        let model = local.model()?;
        let seen = stamps(&model.sources);
        let s = model.summary();
        println!("{}", dim(&format!("{}/{} here: {} tables, {} fields, {} joins, {} policies, loaded in {:.0} ms", model.uid, model.branch, s["tables"], s["fields"], s["joins"], s["policies"], t0.elapsed().as_secs_f64() * 1e3)));
        for w in &model.warnings {
            println!("{}", dim(&format!("warning: {w}")));
        }
        Ok(Session { local, model, seen })
    }
}

impl Side for Session {
    fn origin(&self) -> &'static str {
        "local"
    }

    fn announce(&mut self) -> Result<Vec<String>> {
        Ok(names_in(&self.model.fields("", true)))
    }

    fn refresh(&mut self) -> Result<bool> {
        if stamps(&self.model.sources) == self.seen {
            return Ok(false);
        }
        self.reload()?;
        Ok(true)
    }

    fn reload(&mut self) -> Result<()> {
        let t0 = Instant::now();
        match self.local.model() {
            Ok(m) => {
                self.model = m;
                self.seen = stamps(&self.model.sources);
                println!("{}", dim(&format!("reloaded in {:.0} ms", t0.elapsed().as_secs_f64() * 1e3)));
                for w in &self.model.warnings {
                    println!("{}", dim(&format!("warning: {w}")));
                }
            }
            Err(e) => {
                self.seen = stamps(&self.model.sources);
                println!("error: {e}");
                println!("{}", dim("still on the model loaded before; fix the file and it loads again"));
            }
        }
        Ok(())
    }

    fn branch(&mut self, to: Option<&str>) -> Result<bool> {
        match to {
            None => println!("{} (the files in {})", self.model.branch, self.local.project.dir.display()),
            Some(b) => println!("{}", dim(&format!("the files here are what they are; check out {b} to plan it, or `zsql -r repl --branch {b}` for the server's copy"))),
        }
        Ok(false)
    }

    fn tables(&mut self) -> Result<Value> {
        Ok(self.model.tables())
    }

    fn fields(&mut self, q: &str) -> Result<Value> {
        Ok(self.model.fields(q, false))
    }

    fn values(&mut self, dim: &str) -> Result<Value> {
        local::values_for(&self.model, dim)
    }

    fn explore(&mut self, spec: Option<&Value>, expr: Option<&str>, context: Option<&Value>, term: Option<&str>) -> Result<Value> {
        local::explore(&self.model, spec.cloned(), expr, context, term)
    }

    fn sql(&mut self, spec: Option<&Value>, expr: Option<&str>, context: Option<&Value>, explain: bool) -> Result<Value> {
        local::sql(&self.model, spec.cloned(), expr, context, explain)
    }
}

fn names_in(fields: &Value) -> Vec<String> {
    fields["fields"].as_array().into_iter().flatten().flat_map(|f| [f["uid"].as_str().unwrap_or("").to_lowercase(), f["name"].as_str().unwrap_or("").to_lowercase()]).collect()
}

fn tty() -> bool {
    std::io::stdout().is_terminal()
}

fn dim(s: &str) -> String {
    if tty() {
        format!("\x1b[2m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn bold(s: &str) -> String {
    if tty() {
        format!("\x1b[1m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

// -------------------------------------------------------------------- loop

pub fn run(mut side: impl Side, context: Option<Value>) -> Result<()> {
    let mut field_names = side.announce()?;
    println!("{}", dim("type a line of projections and filters; .help for the forms, .quit to leave"));

    let mut editor: Option<rustyline::DefaultEditor> = if std::io::stdin().is_terminal() { rustyline::DefaultEditor::new().ok() } else { None };
    let history = config::home().map(|h| h.join("history"));
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
        if side.refresh()? {
            field_names = side.announce()?;
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
                "help" | "?" => println!("{}", zsql_shorthand::HELP),
                "tables" => show::print_tables(&side.tables()?),
                "fields" => show::print_fields(&side.fields(rest)?),
                "values" => {
                    if rest.is_empty() {
                        println!("usage: .values <dimension>");
                    } else {
                        show::print_values(&side.values(rest)?);
                    }
                }
                "branch" => {
                    if side.branch(if rest.is_empty() { None } else { Some(rest) })? {
                        field_names = side.announce()?;
                    }
                }
                "reload" => {
                    side.reload()?;
                    field_names = side.announce()?;
                }
                "spec" => match zsql_service::spec_of(None, Some(rest), &zsql_service::unlimited).map_err(Refusal::from) {
                    Ok((spec, _, items)) => {
                        for d in items {
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
                    let text = if cmd.is_empty() { line } else { rest };
                    let (text, term) = match cmd {
                        "explore" => text.rsplit_once(" ? ").map(|(l, q)| (l.trim(), Some(q.trim()))).unwrap_or((text, None)),
                        _ => (text, None),
                    };
                    let (spec, expr): (Option<Value>, Option<&str>) = if cmd == "json" || text.starts_with('{') {
                        match serde_json::from_str(text) {
                            Ok(v) => (Some(v), None),
                            Err(e) => {
                                println!("error: {e}");
                                return Ok(());
                            }
                        }
                    } else {
                        (None, Some(text))
                    };
                    let origin = side.origin();
                    if cmd == "explore" {
                        match side.explore(spec.as_ref(), expr, context.as_ref(), term) {
                            Ok(v) => show::print_explore(&v, &dim, origin),
                            Err(e) => println!("error: {e}"),
                        }
                        return Ok(());
                    }
                    let t0 = Instant::now();
                    match side.sql(spec.as_ref(), expr, context.as_ref(), cmd == "explain") {
                        Ok(v) if cmd == "explain" => show::print_explain(&v, &dim, origin),
                        Ok(v) => {
                            show::print_sql(&v, &dim, &bold);
                            let took = t0.elapsed().as_secs_f64() * 1e3;
                            println!("{}", dim(&if origin == "server" { format!("-- round trip {took:.2} ms") } else { format!("-- planned here in {took:.2} ms") }));
                        }
                        Err(e) => println!("error: {e}"),
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

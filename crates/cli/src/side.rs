//! The two sides a command can run on, behind one trait: the project
//! directory at hand, loaded and held, or a server's copy of the
//! model. Every read a command or the repl makes goes through here, so
//! the two cannot drift apart.

use crate::failure::Refusal;
use crate::local::{self, Local};
use crate::project::{stamps, stamps_under, Stamp};
use crate::{remote, Target};
use anyhow::Result;
use serde_json::Value;
use std::time::Instant;
use zsql_service::Model;

/// A query as it travels: a spec, or the expression line for whoever
/// plans it to read, so the item descriptions come from one place.
#[derive(Debug, Clone)]
pub enum Query {
    Spec(Value),
    Expr(String),
}

impl Query {
    pub fn spec(&self) -> Option<&Value> {
        match self {
            Query::Spec(v) => Some(v),
            Query::Expr(_) => None,
        }
    }

    pub fn expr(&self) -> Option<&str> {
        match self {
            Query::Spec(_) => None,
            Query::Expr(e) => Some(e),
        }
    }
}

/// What a command asks of whichever side it is on. The reads take
/// `&mut self` because the local side may reload underneath them.
pub trait Side {
    /// `server` or `local`: who planned it, for the footer.
    fn origin(&self) -> &'static str;
    /// Prints the summary line; answers with every field's uid and
    /// name, lower-cased, so a bare `help` can tell a field from a word.
    fn announce(&mut self) -> Result<Vec<String>>;
    /// Called before each line of the repl. True when the model was
    /// reloaded and the field names should be read again.
    fn refresh(&mut self) -> Result<bool>;
    /// `.reload`: load again whether or not anything moved.
    fn reload(&mut self) -> Result<()>;
    /// `.branch`: show the branch, or switch to one; true when switched.
    fn branch(&mut self, to: Option<&str>) -> Result<bool>;
    fn tables(&mut self) -> Result<Value>;
    fn fields(&mut self, q: &str, hidden: bool) -> Result<Value>;
    fn values(&mut self, dim: &str) -> Result<Value>;
    fn explore(&mut self, query: &Query, context: Option<&Value>, term: Option<&str>) -> Result<Value>;
    fn sql(&mut self, query: &Query, context: Option<&Value>, explain: bool) -> Result<Value>;
    /// What `check` answers with: the summary, the test results.
    fn check(&mut self) -> Result<Value>;
    fn test(&mut self) -> Result<Value>;
    /// `.run`: the query run on its datasource and its rows kept, which
    /// only this machine can do, holding the credentials.
    fn run(&mut self, _query: &Query, _context: Option<&Value>, _fresh: bool) -> Result<()> {
        anyhow::bail!(
            "runs here only: the credentials are on this machine, and the server holds none. Open the repl without -r to run queries."
        )
    }
    /// `.results`: what `.run` kept.
    fn results(&mut self) -> Result<()> {
        anyhow::bail!("results are kept here only; open the repl without -r to list them")
    }
}

// ------------------------------------------------------------------ remote

impl Side for Target {
    fn origin(&self) -> &'static str {
        "server"
    }

    fn announce(&mut self) -> Result<Vec<String>> {
        let summary = self.client.get(&remote::branch_path(&self.uid, &self.branch))?;
        println!(
            "{}",
            dim(&format!(
                "{}/{} on {}: {} tables, {} fields, {} joins, {} policies",
                self.uid, self.branch, self.client.server, summary["tables"], summary["fields"], summary["joins"], summary["policies"]
            ))
        );
        Ok(names_in(&self.fields("", true)?))
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

    fn fields(&mut self, q: &str, hidden: bool) -> Result<Value> {
        self.client.get(&format!("{}/fields?q={}&hidden={hidden}", remote::branch_path(&self.uid, &self.branch), remote::urlencode(q)))
    }

    fn values(&mut self, dim: &str) -> Result<Value> {
        remote::values_for(&self.client, &self.uid, &self.branch, dim)
    }

    fn explore(&mut self, query: &Query, context: Option<&Value>, term: Option<&str>) -> Result<Value> {
        remote::explore(&self.client, &self.uid, &self.branch, query, context, term)
    }

    fn sql(&mut self, query: &Query, context: Option<&Value>, explain: bool) -> Result<Value> {
        remote::sql(&self.client, &self.uid, &self.branch, query, context, explain)
    }

    fn check(&mut self) -> Result<Value> {
        let Some(p) = &self.project else { anyhow::bail!("validating on the server needs a project directory") };
        let archive = crate::project::archive(&p.dir)?;
        self.client.post_bytes(&format!("{}/validate", remote::branch_path(&self.uid, &self.branch)), "application/gzip", &archive)
    }

    fn test(&mut self) -> Result<Value> {
        self.client.post(&format!("{}/test", remote::branch_path(&self.uid, &self.branch)), &serde_json::json!({}))
    }
}

// ------------------------------------------------------------------- local

/// The project directory, loaded and held.
pub struct Session {
    local: Local,
    model: Model,
    /// The sources as they were when the model was loaded. After a
    /// failed reload it is every model file under the project instead,
    /// since the file at fault may be one the old model never read.
    seen: Vec<Stamp>,
    failed: bool,
}

impl Session {
    /// Loads the project; the summary line is printed when `announce`
    /// is asked for it.
    pub fn open(local: Local) -> Result<Session> {
        let t0 = Instant::now();
        let model = local.model()?;
        let seen = stamps(&model.sources);
        let s = model.summary();
        println!(
            "{}",
            dim(&format!(
                "{}/{} here: {} tables, {} fields, {} joins, {} policies, loaded in {:.0} ms",
                model.uid,
                model.branch,
                s["tables"],
                s["fields"],
                s["joins"],
                s["policies"],
                t0.elapsed().as_secs_f64() * 1e3
            ))
        );
        for w in &model.warnings {
            println!("{}", dim(&format!("warning: {w}")));
        }
        Ok(Session { local, model, seen, failed: false })
    }

    /// Loads the project once, for a command that asks one thing of it.
    pub fn quiet(local: Local) -> Result<Session> {
        let model = local.model()?;
        let seen = stamps(&model.sources);
        Ok(Session { local, model, seen, failed: false })
    }

    fn current(&self) -> Vec<Stamp> {
        if self.failed {
            stamps_under(&self.local.project.dir)
        } else {
            stamps(&self.model.sources)
        }
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
        if self.current() == self.seen {
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
                self.failed = false;
                println!("{}", dim(&format!("reloaded in {:.0} ms", t0.elapsed().as_secs_f64() * 1e3)));
                for w in &self.model.warnings {
                    println!("{}", dim(&format!("warning: {w}")));
                }
            }
            Err(e) => {
                self.failed = true;
                println!("error: {e}");
                println!("{}", dim("still on the model loaded before; fix the file and it loads again"));
            }
        }
        self.seen = self.current();
        Ok(())
    }

    fn branch(&mut self, to: Option<&str>) -> Result<bool> {
        match to {
            None => println!("{} (the files in {})", self.model.branch, self.local.project.dir.display()),
            Some(b) => println!(
                "{}",
                dim(&format!(
                    "the files here are what they are; check out {b} to plan it, or `zsql -r repl --branch {b}` for the server's copy"
                ))
            ),
        }
        Ok(false)
    }

    fn tables(&mut self) -> Result<Value> {
        Ok(self.model.tables())
    }

    fn fields(&mut self, q: &str, hidden: bool) -> Result<Value> {
        Ok(self.model.fields(q, hidden))
    }

    fn values(&mut self, dim: &str) -> Result<Value> {
        Ok(self.model.values_of(dim).map_err(Refusal::from)?)
    }

    fn explore(&mut self, query: &Query, context: Option<&Value>, term: Option<&str>) -> Result<Value> {
        local::explore(&self.model, query, context, term)
    }

    fn sql(&mut self, query: &Query, context: Option<&Value>, explain: bool) -> Result<Value> {
        local::sql(&self.model, query, context, explain)
    }

    fn run(&mut self, query: &Query, context: Option<&Value>, fresh: bool) -> Result<()> {
        crate::run::run(&self.local.project, &self.model, query, context, crate::run::Options { fresh, ..Default::default() })
    }

    fn results(&mut self) -> Result<()> {
        crate::run::results(&self.local.project, crate::run::ResultsCommand::List { json: false })
    }

    fn check(&mut self) -> Result<Value> {
        local::check(&self.model)
    }

    fn test(&mut self) -> Result<Value> {
        Ok(self.model.run_tests().map_err(Refusal::from)?)
    }
}

fn names_in(fields: &Value) -> Vec<String> {
    fields["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|f| [f["uid"].as_str().unwrap_or("").to_lowercase(), f["name"].as_str().unwrap_or("").to_lowercase()])
        .collect()
}

fn tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

/// Dim and bold, when stdout is a terminal.
pub fn dim(s: &str) -> String {
    if tty() {
        format!("\x1b[2m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn bold(s: &str) -> String {
    if tty() {
        format!("\x1b[1m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

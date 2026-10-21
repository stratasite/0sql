//! Planning here: the project directory at hand, loaded into a model,
//! answering the same questions a server does and in the same shapes.
//!
//! Every function hands back the JSON the matching server route would,
//! built from the same service types, so the printers in `show.rs`
//! cannot tell the two apart and neither can `--json`.

use crate::config::Project;
use crate::failure::Refusal;
use crate::side::Query;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::sync::OnceLock;
use zsql_dialect::Dialects;
use zsql_service::{unlimited, ExploreRequest, Model, SqlRequest, Trust};
use zsql_spec::SecurityContext;

/// What a local model is called where a server would say the branch:
/// the files as they are, whatever is checked out or not.
pub const BRANCH: &str = "working-tree";

/// The dialect settings compiled in, leaked once: the planner borrows
/// them for as long as its model lives, which is the process.
pub fn dialects() -> &'static Dialects {
    static D: OnceLock<&'static Dialects> = OnceLock::new();
    D.get_or_init(|| Box::leak(Box::new(Dialects::builtin())))
}

/// A project directory as the target of a command.
pub struct Local {
    pub project: Project,
}

impl Local {
    /// The project's files, loaded. Said as `uid/working-tree`.
    pub fn model(&self) -> Result<Model> {
        let mut m = Model::from_dir(&self.project.dir, Trust::Owned, dialects())
            .map_err(|e| Refusal { class: "Project".into(), message: e.to_string() })?;
        m.uid = self.project.uid.clone();
        m.branch = BRANCH.into();
        Ok(m)
    }
}

/// The security context, from the JSON a `--context` file holds.
fn context_of(v: Option<&Value>) -> Result<Option<SecurityContext>> {
    match v {
        Some(v) => Ok(Some(serde_json::from_value(v.clone()).context("the security context does not parse")?)),
        None => Ok(None),
    }
}

/// `/sql` and `/explain`, planned here.
pub fn sql(m: &Model, query: &Query, context: Option<&Value>, explain: bool) -> Result<Value> {
    let req = SqlRequest { spec: query.spec().cloned(), expr: query.expr().map(str::to_string), context: context_of(context)? };
    let v = if explain {
        serde_json::to_value(m.explain(req, &unlimited).map_err(Refusal::from)?)?
    } else {
        serde_json::to_value(m.sql(req, &unlimited).map_err(Refusal::from)?)?
    };
    Ok(v)
}

/// `/explore`, planned here.
pub fn explore(m: &Model, query: &Query, context: Option<&Value>, q: Option<&str>) -> Result<Value> {
    let req = ExploreRequest {
        spec: query.spec().cloned(),
        expr: query.expr().map(str::to_string),
        context: context_of(context)?,
        q: q.map(str::trim).filter(|q| !q.is_empty()).map(str::to_string),
    };
    Ok(serde_json::to_value(m.explore(req, &unlimited).map_err(Refusal::from)?)?)
}

/// What a validate answers with: the summary, the test results, and
/// that nothing was kept.
pub fn check(m: &Model) -> Result<Value> {
    let mut out = m.summary();
    out["test_results"] = m.run_tests().map_err(Refusal::from)?;
    out["validated_only"] = json!(true);
    // Against the warehouse's catalogue too, for every datasource that has one.
    out["catalogue"] = crate::catalogue_check::findings(m);
    Ok(out)
}

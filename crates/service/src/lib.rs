//! The service layer: a loaded model, and every read over it.
//!
//! The server keeps one [`Model`] per deployed branch and answers each
//! route from it; the CLI builds one from the project directory at hand
//! and answers the same questions without a server. Both call the
//! functions here, so what a caller gets back, the SQL, the explain
//! graph, the field catalogue, a refusal and its class, is the same
//! whichever binary produced it. The server adds only what is about
//! the server: who may read which branch, rate limits, and the bounds
//! it puts on a request's shape; those arrive here as a [`ShapeCheck`]
//! the caller supplies.
//!
//! The planner is "the engine" throughout this repository; this crate
//! is the layer between a caller and the engine, not the engine.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;
use zsql_dialect::Dialects;
use zsql_semantic::{FieldKind, Snapshot, Universe};
use zsql_spec::SecurityContext;

pub use zsql_project::values::ValueCatalog;
pub use zsql_project::{Loaded, ProjectTest};

// ------------------------------------------------------------------ errors

/// What kind of refusal this is. The server turns it into a status;
/// the CLI into an exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The request itself is malformed or missing something.
    Invalid,
    /// The request is well formed but the model cannot answer it.
    Unprocessable,
    NotFound,
    /// Something this side should have had and did not.
    Internal,
}

/// A refusal, with the class the goldens and the API name it by.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{class}: {message}")]
pub struct ServiceError {
    pub kind: Kind,
    pub class: &'static str,
    pub message: String,
}

impl ServiceError {
    pub fn new(kind: Kind, class: &'static str, message: impl Into<String>) -> Self {
        ServiceError { kind, class, message: message.into() }
    }
    pub fn invalid(class: &'static str, message: impl Into<String>) -> Self {
        Self::new(Kind::Invalid, class, message)
    }
    pub fn unprocessable(class: &'static str, message: impl Into<String>) -> Self {
        Self::new(Kind::Unprocessable, class, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(Kind::NotFound, "NotFound", message)
    }
    pub fn internal(class: &'static str, message: impl Into<String>) -> Self {
        Self::new(Kind::Internal, class, message)
    }
}

impl From<zsql_spec::SpecError> for ServiceError {
    fn from(e: zsql_spec::SpecError) -> Self {
        Self::unprocessable(e.error_class(), e.to_string())
    }
}

impl From<zsql_planner::PlanError> for ServiceError {
    fn from(e: zsql_planner::PlanError) -> Self {
        Self::unprocessable(e.error_class(), e.to_string())
    }
}

impl From<zsql_dialect::DialectError> for ServiceError {
    fn from(e: zsql_dialect::DialectError) -> Self {
        Self::internal("Dialect", e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, ServiceError>;

/// Why a project directory did not become a model.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("{0}")]
    Project(#[from] zsql_project::ProjectError),
    #[error("{0}")]
    Universe(#[from] zsql_semantic::BuildError),
}

/// Why a stored model (what [`Model::write`] put on disk) did not load.
#[derive(Debug, thiserror::Error)]
pub enum StoredError {
    #[error("{0}")]
    Snapshot(#[from] zsql_semantic::snapshot::LoadError),
    #[error("{0}")]
    Universe(#[from] zsql_semantic::BuildError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("tests.json does not parse: {0}")]
    Tests(String),
}

// ------------------------------------------------------------------- model

/// Whose files a project directory holds. The server unpacks an
/// archive someone sent and must not follow a link or an import out of
/// it; the CLI reads the developer's own machine and keeps the freedom.
/// A type rather than a flag so a caller has to say which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    Owned,
    Untrusted,
}

/// Loads a project directory as the trust level says.
pub fn load(dir: &Path, trust: Trust, dialects: &Dialects) -> std::result::Result<Loaded, zsql_project::ProjectError> {
    match trust {
        Trust::Owned => zsql_project::load(dir, dialects),
        Trust::Untrusted => zsql_project::load_confined(dir, dialects),
    }
}

// A model's universe and the planner index built over it, in one cell:
// the planner borrows the universe, so the two must live and die
// together, and the cell keeps the universe where the borrow expects
// it. Built once per load; every request borrows it.
self_cell::self_cell!(
    struct Engine {
        owner: Universe,
        #[covariant]
        dependent: Planner,
    }
);

type Planner<'a> = zsql_planner::Planner<'a>;

/// One branch of one project, ready to answer: the universe, the
/// planner over it, the sampled values and the tests it came with.
pub struct Model {
    pub uid: String,
    pub branch: String,
    engine: Engine,
    pub dialects: &'static Dialects,
    pub values: Option<ValueCatalog>,
    pub tests: Vec<ProjectTest>,
    pub warnings: Vec<String>,
    /// When it was loaded, as `utc_minute` writes it; the summary calls
    /// it `deployed_at`, which is what it is on a server.
    pub loaded_at: String,
    /// What a load from a directory depends on, for whoever wants to
    /// know when it moves. Empty for a model built any other way.
    pub sources: Vec<PathBuf>,
    /// Each field's tables, by field id, in model order: built once,
    /// since the catalogue asks for it once per field.
    tables_by_field: HashMap<zsql_semantic::Id, Vec<String>>,
}

/// What the files on disk are called under a model's directory.
pub const SNAPSHOT_FILE: &str = "snapshot.json.gz";
pub const TESTS_FILE: &str = "tests.json";

impl Model {
    /// Over a snapshot alone: no values, no tests, the universe's own
    /// warnings.
    pub fn build(
        uid: &str,
        branch: &str,
        snapshot: Snapshot,
        dialects: &'static Dialects,
    ) -> std::result::Result<Model, zsql_semantic::BuildError> {
        let universe = Universe::build(snapshot)?;
        let warnings = universe.warnings().to_vec();
        let mut tables_by_field: HashMap<zsql_semantic::Id, Vec<String>> = HashMap::new();
        for e in &universe.snapshot().expressions {
            if let Some(t) = universe.table(e.table_id) {
                tables_by_field.entry(e.field_id).or_default().push(t.uid.clone());
            }
        }
        let engine = Engine::new(universe, |u| Planner::new(u, dialects, None));
        Ok(Model {
            uid: uid.to_string(),
            branch: branch.to_string(),
            engine,
            dialects,
            values: None,
            tests: vec![],
            warnings,
            loaded_at: now(),
            sources: vec![],
            tables_by_field,
        })
    }

    /// Over what the loader read: its warnings first, then the universe's.
    pub fn from_loaded(
        uid: &str,
        branch: &str,
        loaded: Loaded,
        dialects: &'static Dialects,
    ) -> std::result::Result<Model, zsql_semantic::BuildError> {
        let mut m = Self::build(uid, branch, loaded.snapshot, dialects)?;
        let mut warnings = loaded.warnings;
        warnings.append(&mut m.warnings);
        m.warnings = warnings;
        m.values = loaded.values;
        m.tests = loaded.tests;
        m.sources = loaded.sources;
        Ok(m)
    }

    /// A project directory, named by its own project.yml and production
    /// branch. The caller may rename either afterwards.
    pub fn from_dir(dir: &Path, trust: Trust, dialects: &'static Dialects) -> std::result::Result<Model, LoadError> {
        let loaded = load(dir, trust, dialects)?;
        let (uid, branch) = (loaded.snapshot.project.uid.clone(), loaded.snapshot.branch.uid.clone());
        Ok(Self::from_loaded(&uid, &branch, loaded, dialects)?)
    }

    /// What [`Model::write`] put under `dir`. `loaded_at` is the
    /// snapshot file's modification time.
    pub fn from_stored(dir: &Path, uid: &str, branch: &str, dialects: &'static Dialects) -> std::result::Result<Model, StoredError> {
        let snap_path = dir.join(SNAPSHOT_FILE);
        let snapshot = Snapshot::from_path(&snap_path)?;
        let mut m = Self::build(uid, branch, snapshot, dialects)?;
        m.warnings.clear();
        m.values = ValueCatalog::read(&dir.join(zsql_project::values::FILE_NAME))?;
        m.tests = match std::fs::read_to_string(dir.join(TESTS_FILE)) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| StoredError::Tests(e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(e.into()),
        };
        m.loaded_at = std::fs::metadata(&snap_path).and_then(|m| m.modified()).map(utc_minute).unwrap_or_default();
        Ok(m)
    }

    /// Keeps the compiled model under `dir` so [`Model::from_stored`]
    /// can bring it back: the snapshot, the sampled values when there
    /// are any (and not a stale file when there are none), and the
    /// tests.
    pub fn write(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        write_gz(&dir.join(SNAPSHOT_FILE), &serde_json::to_vec(self.universe().snapshot())?)?;
        let values_path = dir.join(zsql_project::values::FILE_NAME);
        match &self.values {
            Some(v) => v.write(&values_path)?,
            None => {
                let _ = std::fs::remove_file(values_path);
            }
        }
        // ProjectTest serialises its file as a file name, so nothing of
        // the directory it was loaded from is kept.
        std::fs::write(dir.join(TESTS_FILE), serde_json::to_vec(&self.tests)?)
    }

    pub fn universe(&self) -> &Universe {
        self.engine.borrow_owner()
    }

    pub fn planner(&self) -> &Planner<'_> {
        self.engine.borrow_dependent()
    }

    /// Counts and warnings: what a deploy answers with, and what
    /// `GET /projects/{uid}/branches/{branch}` says.
    pub fn summary(&self) -> Value {
        let s = self.universe().snapshot();
        json!({
            "project": self.uid, "branch": self.branch, "deployed_at": self.loaded_at,
            "datasources": s.datasources.len(), "tables": s.tables.len(), "fields": s.fields.len(),
            "joins": s.join_defs.len(), "paths": s.paths.len(), "policies": s.security_policies.len(),
            "values": self.values.as_ref().map(|v| v.dimensions.len()).unwrap_or(0),
            "tests": self.tests.len(),
            "warnings": self.warnings,
        })
    }

    /// Runs the project's tests/*.yml: each projects its fields and
    /// compares the SQL with the assertion.
    pub fn run_tests(&self) -> Result<Value> {
        let planner = self.planner();
        let base = self.dialects.base()?;
        let normalize = |sql: &str| sql.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
        let mut results = Vec::new();
        let (mut passed, mut failed) = (0, 0);
        for t in &self.tests {
            let spec = zsql_spec::Spec {
                projections: t
                    .projections
                    .iter()
                    .map(|f| zsql_spec::spec::ProjectionSpec { field: Some(Value::String(f.clone())), ..Default::default() })
                    .collect(),
                ..Default::default()
            };
            let outcome = zsql_spec::lower(&spec, self.universe(), &base)
                .map_err(|e| e.to_string())
                .and_then(|q| planner.plan_query(q, None).map(|p| p.sql).map_err(|e| e.to_string()));
            let (status, detail) = match (&outcome, &t.assert_sql, &t.assert_regex) {
                (Err(e), _, _) => ("error", json!({ "message": e })),
                (Ok(sql), Some(expected), _) => {
                    if normalize(sql) == normalize(expected) {
                        ("passed", Value::Null)
                    } else {
                        (
                            "failed",
                            json!({ "message": "Generated SQL does not match expected SQL", "expected_sql": expected, "generated_sql": sql }),
                        )
                    }
                }
                (Ok(sql), None, Some(pattern)) => {
                    match regex::RegexBuilder::new(pattern).case_insensitive(true).dot_matches_new_line(true).build() {
                        Ok(re) if re.is_match(sql) => ("passed", Value::Null),
                        Ok(_) => (
                            "failed",
                            json!({ "message": "Generated SQL does not match regex pattern", "pattern": pattern, "generated_sql": sql }),
                        ),
                        Err(e) => ("error", json!({ "message": format!("bad assert_regex: {e}") })),
                    }
                }
                (Ok(_), None, None) => ("error", json!({ "message": "No assertion defined (assert_sql or assert_regex required)" })),
            };
            if status == "passed" {
                passed += 1;
            } else {
                failed += 1;
            }
            let mut r = json!({ "name": t.name, "status": status, "file": t.file.file_name().map(|f| f.to_string_lossy().to_string()) });
            if let Value::Object(extra) = detail {
                for (k, v) in extra {
                    r[k] = v;
                }
            }
            results.push(r);
        }
        Ok(json!({ "passed": passed, "failed": failed, "results": results }))
    }

    // ------------------------------------------------------- sql, explain

    /// Resolves and plans one request. `explain` also collects the
    /// phase timings and the node graph.
    pub fn plan_request(&self, req: SqlRequest, explain: bool, check: ShapeCheck) -> Result<Planned> {
        if req.context.is_none() && !self.universe().policies().is_empty() {
            return Err(ServiceError::invalid("ContextRequired", "this branch has security policies; a context is required to plan"));
        }
        let base = self.dialects.base()?;
        let (spec_value, made_spec, items) = spec_of(req.spec, req.expr.as_deref(), check)?;
        let t0 = Instant::now();
        let spec = zsql_spec::Spec::from_value(spec_value)?;
        let mut query = zsql_spec::lower(&spec, self.universe(), &base)?;
        let corrections = std::mem::take(&mut query.corrections);
        // Planning does not read the limit, so take it here while the query is
        // still whole. A zero or negative limit is nothing to emit.
        let limit = query.limit.and_then(|n| u64::try_from(n).ok()).filter(|n| *n > 0);
        let t1 = Instant::now();
        let plan = self.planner().plan_query_explained(query, req.context.as_ref(), explain)?;
        let t2 = Instant::now();
        Ok(Planned {
            plan,
            corrections,
            timings: Timings { parser_us: (t1 - t0).as_micros(), plan_us: (t2 - t1).as_micros(), total_us: (t2 - t0).as_micros() },
            spec: made_spec,
            items,
            limit,
        })
    }

    pub fn sql(&self, req: SqlRequest, check: ShapeCheck) -> Result<SqlResponse> {
        let p = self.plan_request(req, false, check)?;
        let dialect = self.dialects.get(&p.plan.adapter)?;
        Ok(SqlResponse {
            sql: p.plan.sql_with_limit(p.limit, &dialect)?,
            datasource: p.plan.datasource,
            datasource_uid: p.plan.datasource_uid,
            adapter: p.plan.adapter,
            corrections: p.corrections,
            spec: p.spec,
            items: p.items,
        })
    }

    pub fn explain(&self, req: SqlRequest, check: ShapeCheck) -> Result<ExplainResponse> {
        let p = self.plan_request(req, true, check)?;
        // Before taking `explain` out of the plan, which moves part of it.
        let dialect = self.dialects.get(&p.plan.adapter)?;
        let sql = p.plan.sql_with_limit(p.limit, &dialect)?;
        let x = p.plan.explain.unwrap_or_else(|| zsql_planner::Explain { phases: vec![], nodes: vec![] });
        Ok(ExplainResponse {
            sql,
            datasource: p.plan.datasource,
            datasource_uid: p.plan.datasource_uid,
            adapter: p.plan.adapter,
            corrections: p.corrections,
            spec: p.spec,
            items: p.items,
            timings: p.timings,
            phases: x.phases,
            nodes: x.nodes,
        })
    }

    /// What a query can take on: the visible dimensions and measures the
    /// planner's universes can add to it, from a walk of the formed
    /// universes, narrowed by a search term when one is given.
    pub fn explore(&self, req: ExploreRequest, check: ShapeCheck) -> Result<ExploreResponse> {
        let base = self.dialects.base()?;
        let (spec_value, made, items) = spec_of(req.spec, req.expr.as_deref(), check)?;
        let t0 = Instant::now();
        let spec = zsql_spec::Spec::from_value(spec_value)?;
        let query = zsql_spec::lower(&spec, self.universe(), &base)?;
        let x = self.planner().explore(query)?;
        let q = req.q.as_deref().map(zsql_spec::fuzzy::normalize).filter(|q| !q.is_empty());
        let wanted = |f: &zsql_semantic::Field| -> Option<f64> {
            let Some(q) = &q else { return Some(1.0) };
            let forms = std::iter::once(&f.name).chain(std::iter::once(&f.uid)).chain(f.synonyms.iter());
            let mut best = 0.0f64;
            for form in forms {
                let n = zsql_spec::fuzzy::normalize(form);
                if n.contains(q.as_str()) {
                    return Some(1.0);
                }
                best = best.max(zsql_spec::fuzzy::similarity(q, &n));
            }
            (best >= zsql_spec::fuzzy::FLOOR).then_some(best)
        };
        let describe = |fields: Vec<&zsql_semantic::Field>| -> Vec<Value> {
            let mut out: Vec<(f64, Value)> = fields
                .into_iter()
                .filter(|f| !f.hidden)
                .filter_map(|f| {
                    let score = wanted(f)?;
                    let mut v = json!({
                        "uid": f.uid, "name": f.name, "data_type": f.data_type, "description": f.description,
                        "tables": self.tables_of(f.id),
                    });
                    if q.is_some() {
                        v["score"] = json!((score * 100.0).round() / 100.0);
                    }
                    Some((score, v))
                })
                .collect();
            if q.is_some() {
                out.sort_by(|a, b| b.0.total_cmp(&a.0));
            }
            out.into_iter().map(|(_, v)| v).collect()
        };
        Ok(ExploreResponse {
            dimensions: describe(x.dimensions),
            measures: describe(x.measures),
            total_us: t0.elapsed().as_micros(),
            spec: made,
            items,
        })
    }

    // ---------------------------------------------------------- discovery

    /// The uids of the tables carrying a field, in model order.
    fn tables_of(&self, field_id: zsql_semantic::Id) -> Vec<String> {
        self.tables_by_field.get(&field_id).cloned().unwrap_or_default()
    }

    /// Field discovery. With `q`, fields containing the text come first in
    /// model order, then near matches by trigram similarity, each with its
    /// `score` (1 for a containing match).
    pub fn fields(&self, q: &str, hidden: bool) -> Value {
        let needle = q.trim().to_lowercase();
        let visible = |f: &&zsql_semantic::Field| hidden || !f.hidden;
        let describe = |f: &zsql_semantic::Field, score: Option<f64>| {
            let mut v = json!({
                "uid": f.uid, "name": f.name,
                "kind": match f.kind { FieldKind::Dimension => "dimension", FieldKind::Measure => "measure" },
                "data_type": f.data_type, "description": f.description, "synonyms": f.synonyms, "tags": f.tags, "hidden": f.hidden,
                "tables": self.tables_of(f.id),
            });
            if let Some(s) = score {
                v["score"] = json!((s * 100.0).round() / 100.0);
            }
            v
        };
        if needle.is_empty() {
            return json!({ "fields": self.universe().fields().iter().filter(visible).map(|f| describe(f, None)).collect::<Vec<_>>() });
        }
        let mut out: Vec<Value> = self
            .universe()
            .fields()
            .iter()
            .filter(visible)
            .filter(|f| {
                f.uid.to_lowercase().contains(&needle)
                    || f.name.to_lowercase().contains(&needle)
                    || f.synonyms.iter().any(|s| s.to_lowercase().contains(&needle))
            })
            .map(|f| describe(f, Some(1.0)))
            .collect();
        let seen: std::collections::HashSet<String> = out.iter().filter_map(|v| v["uid"].as_str()).map(str::to_string).collect();
        for s in zsql_spec::fuzzy::suggest(self.universe(), &needle, &[FieldKind::Dimension, FieldKind::Measure]) {
            if s.score < zsql_spec::fuzzy::FLOOR / 2.0 {
                break;
            }
            if visible(&s.field) && !seen.contains(&s.field.uid) {
                out.push(describe(s.field, Some(s.score)));
            }
        }
        json!({ "fields": out })
    }

    pub fn tables(&self) -> Value {
        let u = self.universe();
        let out: Vec<Value> = u
            .tables()
            .iter()
            .map(|t| {
                json!({
                    "uid": t.uid, "name": t.name, "physical_name": t.physical_name, "cost": t.cost,
                    "datasource": u.datasource(t.datasource_id).map(|x| x.uid.clone()),
                    "fields": u.expressions_of(t.id).filter_map(|e| u.field(e.field_id)).map(|f| f.uid.clone()).collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({ "tables": out })
    }

    /// A dimension's sampled values, by uid or by name, else by a unique
    /// substring of a dimension's uid, name or synonyms (hidden ones
    /// included). Several substring matches are an `Ambiguous` refusal
    /// naming them; a near miss is never taken for the user.
    pub fn values_of(&self, dim: &str) -> Result<Value> {
        let u = self.universe();
        let field = match u.field_by_uid(dim, Some(FieldKind::Dimension)).or_else(|| u.field_by_name(dim, FieldKind::Dimension)) {
            Some(f) => f,
            None => {
                let needle = dim.trim().to_lowercase();
                let contains = |s: &str| !needle.is_empty() && s.to_lowercase().contains(&needle);
                let hits: Vec<&zsql_semantic::Field> = u
                    .fields()
                    .iter()
                    .filter(|f| f.is_dimension() && (contains(&f.uid) || contains(&f.name) || f.synonyms.iter().any(|s| contains(s))))
                    .collect();
                match hits.as_slice() {
                    [one] => *one,
                    [] => return Err(ServiceError::not_found(format!("no dimension matches '{dim}'"))),
                    many => {
                        let uids: Vec<&str> = many.iter().map(|f| f.uid.as_str()).collect();
                        return Err(ServiceError::invalid("Ambiguous", format!("'{dim}' matches several dimensions: {}", uids.join(", "))));
                    }
                }
            }
        };
        match self.values.as_ref().and_then(|v| v.get(&field.uid)) {
            Some(e) => Ok(serde_json::to_value(e).expect("sampled values serialize")),
            None => Err(ServiceError::not_found(format!("no sampled values for {}", field.uid))),
        }
    }
}

// ---------------------------------------------------------------- requests

/// A bound a caller puts on a spec before any work starts. The server
/// refuses one built to be expensive (`docs/rate-limiting.md`); a local
/// caller passes [`unlimited`].
pub type ShapeCheck<'a> = &'a dyn Fn(&Value) -> Result<()>;

/// No bound at all.
pub fn unlimited(_: &Value) -> Result<()> {
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
pub struct SqlRequest {
    /// A query spec, or instead `expr`, a line of the shorthand.
    #[serde(default)]
    pub spec: Option<Value>,
    #[serde(default)]
    pub expr: Option<String>,
    #[serde(default)]
    pub context: Option<SecurityContext>,
}

/// The spec a request names: given, or made from its shorthand line, in
/// which case the made spec and the item descriptions come back too.
pub fn spec_of(spec: Option<Value>, expr: Option<&str>, check: ShapeCheck) -> Result<(Value, Option<Value>, Vec<String>)> {
    match (spec, expr.map(str::trim).filter(|e| !e.is_empty())) {
        (Some(s), _) => {
            check(&s)?;
            Ok((s, None, vec![]))
        }
        (None, Some(line)) => {
            let (spec, items) = zsql_shorthand::parse_line(line).map_err(|m| ServiceError::invalid("Shorthand", m))?;
            check(&spec)?;
            let described = zsql_shorthand::describe(&items);
            Ok((spec.clone(), Some(spec), described))
        }
        (None, None) => Err(ServiceError::invalid("Invalid", "give a spec or an expr")),
    }
}

/// What a query needs to run: the statement and the datasource it is for.
/// `corrections` lists references that resolved by near match.
#[derive(Debug, Serialize)]
pub struct SqlResponse {
    pub sql: String,
    pub datasource: String,
    pub datasource_uid: String,
    pub adapter: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub corrections: Vec<zsql_spec::fuzzy::Correction>,
    /// The spec a shorthand `expr` made, and how each item was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

/// The statement, how long planning took, each planning phase's share,
/// and the node graph the visitors shaped.
#[derive(Debug, Serialize)]
pub struct ExplainResponse {
    pub sql: String,
    pub datasource: String,
    pub datasource_uid: String,
    pub adapter: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub corrections: Vec<zsql_spec::fuzzy::Correction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    pub timings: Timings,
    pub phases: Vec<zsql_planner::Phase>,
    pub nodes: Vec<zsql_planner::ExplainNode>,
}

/// Microseconds spent: parsing and resolving the spec, planning it,
/// and both together.
#[derive(Debug, Clone, Serialize)]
pub struct Timings {
    pub parser_us: u128,
    pub plan_us: u128,
    pub total_us: u128,
}

/// A planned request before it is shaped into an answer.
pub struct Planned {
    pub plan: zsql_planner::Plan,
    pub corrections: Vec<zsql_spec::fuzzy::Correction>,
    pub timings: Timings,
    pub spec: Option<Value>,
    pub items: Vec<String>,
    /// The caller's row limit, taken off the spec before planning. `None`
    /// when they did not send one, and no default is supplied for them.
    pub limit: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ExploreRequest {
    #[serde(default)]
    pub spec: Option<Value>,
    #[serde(default)]
    pub expr: Option<String>,
    /// Accepted for symmetry with sql and explain; exploring resolves
    /// universes only, so no policy applies yet.
    #[serde(default)]
    pub context: Option<SecurityContext>,
    /// Keep only fields whose name, uid or synonyms contain this, or come
    /// close to it.
    #[serde(default)]
    pub q: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ExploreResponse {
    pub dimensions: Vec<Value>,
    pub measures: Vec<Value>,
    pub total_us: u128,
    /// The spec an expression line made, and how each item was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

// -------------------------------------------------------------------- time

/// Now, as `utc_minute` writes it.
pub fn now() -> String {
    utc_minute(std::time::SystemTime::now())
}

/// UTC, to the minute: `2026-10-01 15:04`.
pub fn utc_minute(t: std::time::SystemTime) -> String {
    let secs = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    // Civil-from-days (Howard Hinnant), so no date crate is needed here.
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

fn write_gz(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    enc.write_all(bytes)?;
    enc.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zsql_golden::{Fixtures, PathSource};

    /// A model is shared across threads by the server; nothing added to
    /// it may take that away.
    #[test]
    fn a_model_is_send_and_sync() {
        fn assert<T: Send + Sync>() {}
        assert::<Model>();
    }

    fn dialects() -> &'static Dialects {
        Box::leak(Box::new(Dialects::builtin()))
    }

    /// Every golden case planned over its universe's stored paths, as a
    /// model does: the SQL for each, or the error, in case order.
    fn plan_all(fixtures: &Fixtures, dialects: &'static Dialects) -> Vec<String> {
        let mut out = Vec::new();
        let mut models: std::collections::HashMap<String, Model> = Default::default();
        for cases in fixtures.cases().expect("golden cases").into_values() {
            for case in cases {
                let model = models.entry(case.universe.clone()).or_insert_with(|| {
                    Model::build("g", &case.universe, fixtures.snapshot(&case.universe).expect("snapshot"), dialects).expect("universe")
                });
                let req = SqlRequest { spec: Some(case.spec.clone()), expr: None, context: case.context.clone() };
                out.push(match model.sql(req, &unlimited) {
                    Ok(r) => format!("{}: {}", case.id, r.sql),
                    Err(e) => format!("{}: ERROR {e}", case.id),
                });
            }
        }
        out
    }

    /// The planner numbers projections from a thread-local counter.
    /// Whatever thread plans, and however many plans came before it on
    /// that thread, the SQL must come out the same; otherwise the
    /// server, which plans on a pool, and the CLI, which plans on its
    /// main thread, would disagree.
    #[test]
    fn sql_is_the_same_on_every_thread() {
        let fx = Fixtures::open(Fixtures::default_root());
        let d = dialects();
        let here = plan_all(&fx, d);
        let there = std::thread::spawn(move || {
            // Advance the counter first, so the ids this thread hands
            // out do not line up with the main thread's by accident.
            let fx = Fixtures::open(Fixtures::default_root());
            let _ = plan_all(&fx, d);
            plan_all(&fx, d)
        })
        .join()
        .expect("planning thread");
        assert!(!here.is_empty(), "no golden cases found");
        assert_eq!(here, there);
        let _ = PathSource::Formed;
    }

    /// The same, across processes: a child of this test binary plans
    /// everything and reports a digest, which must match this process's.
    #[test]
    fn sql_is_the_same_in_another_process() {
        const CHILD: &str = "ZSQL_SERVICE_DETERMINISM_CHILD";
        let fx = Fixtures::open(Fixtures::default_root());
        let here = digest(&plan_all(&fx, dialects()));
        if std::env::var_os(CHILD).is_some() {
            println!("DIGEST {here}");
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["tests::sql_is_the_same_in_another_process", "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .expect("child test process");
        let text = String::from_utf8_lossy(&out.stdout);
        // With --nocapture the harness has already printed `test name ...` on the line.
        let there = text
            .lines()
            .find_map(|l| l.find("DIGEST ").map(|i| l[i + 7..].trim()))
            .unwrap_or_else(|| panic!("no digest from the child:\n{text}\n{}", String::from_utf8_lossy(&out.stderr)));
        assert_eq!(here, there);
    }

    fn digest(lines: &[String]) -> String {
        // FNV-1a over the whole text: no dependency, and a mismatch is
        // all the test needs to detect.
        let mut h: u64 = 0xcbf29ce484222325;
        for b in lines.join("\n").bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{h:016x}")
    }

    /// A model written to disk and read back answers the same.
    #[test]
    fn a_stored_model_comes_back_whole() {
        let fx = Fixtures::open(Fixtures::default_root());
        let d = dialects();
        let hash = fx.snapshot_hashes().expect("hashes").into_iter().next().expect("a universe");
        let model = Model::build("p", "b", fx.snapshot(&hash).expect("snapshot"), d).expect("universe");
        let dir = std::env::temp_dir().join(format!("zsql-service-stored-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        model.write(&dir).expect("write");
        let back = Model::from_stored(&dir, "p", "b", d).expect("read back");
        assert_eq!(back.summary()["fields"], model.summary()["fields"]);
        assert_eq!(back.tables(), model.tables());
        assert_eq!(back.fields("", true), model.fields("", true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A refusal names its class and kind the way the API always has.
    #[test]
    fn refusals_carry_their_class() {
        let fx = Fixtures::open(Fixtures::default_root());
        let d = dialects();
        // A universe with policies would refuse everything below for
        // the want of a context, which is its own test.
        let (open, guarded) = fx
            .snapshot_hashes()
            .expect("hashes")
            .into_iter()
            .map(|h| fx.snapshot(&h).expect("snapshot"))
            .fold((None, None), |(o, g), s| if s.security_policies.is_empty() { (o.or(Some(s)), g) } else { (o, g.or(Some(s))) });
        let model = Model::build("p", "b", open.expect("a universe without policies"), d).expect("universe");
        let e = model.sql(SqlRequest::default(), &unlimited).unwrap_err();
        assert_eq!((e.kind, e.class), (Kind::Invalid, "Invalid"));
        let secured = Model::build("p", "b", guarded.expect("a universe with policies"), d).expect("universe");
        let e = secured.sql(SqlRequest { spec: None, expr: Some("anything".into()), context: None }, &unlimited).unwrap_err();
        assert_eq!((e.kind, e.class), (Kind::Invalid, "ContextRequired"));
        let e = model.values_of("no-such-dimension").unwrap_err();
        assert_eq!(e.kind, Kind::NotFound);
        let e = model.sql(SqlRequest { spec: None, expr: Some("no such field at all".into()), context: None }, &unlimited).unwrap_err();
        assert_eq!(e.kind, Kind::Unprocessable, "{e}");
    }
}

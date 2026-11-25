use crate::error::{Error, LoadError, Result, StoredError};
use crate::request::{spec_of, ExplainResponse, ExploreRequest, ExploreResponse, Planned, ShapeCheck, SqlRequest, SqlResponse, Timings};
use crate::time::{now, utc_minute};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;
use zsql_dialect::Dialects;
use zsql_project::values::ValueCatalog;
use zsql_project::{Loaded, ProjectTest};
use zsql_semantic::{FieldKind, SemanticModel, Snapshot};

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
pub fn load(dir: &Path, trust: Trust, dialects: &Dialects) -> std::result::Result<Loaded, zsql_project::Error> {
    match trust {
        Trust::Owned => zsql_project::load(dir, dialects),
        Trust::Untrusted => zsql_project::load_confined(dir, dialects),
    }
}

// A semantic model and the planner index built over it, in one cell:
// the planner borrows the model, so the two must live and die
// together, and the cell keeps the model where the borrow expects
// it. Built once per load; every request borrows it.
self_cell::self_cell!(
    struct Engine {
        owner: SemanticModel,
        #[covariant]
        dependent: Planner,
    }
);

type Planner<'a> = zsql_planner::Planner<'a>;

/// One branch of one project, ready to answer: the semantic model, the
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
    /// since the catalog asks for it once per field.
    tables_by_field: HashMap<zsql_semantic::Id, Vec<String>>,
    /// The secrets each datasource's entry carried, by the datasource's
    /// uid in lower case: every secret key, at the depth it was written,
    /// and nothing else. They are taken out of the snapshot when the
    /// model is built, so the snapshot holds none and can be kept,
    /// compared and passed on as what it is, the compiled model. Kept
    /// apart on disk too, in [`SECRETS_FILE`]. Never served.
    pub secrets: std::collections::BTreeMap<String, serde_json::Map<String, Value>>,
}

/// Where a model's secrets are kept under its directory, readable by
/// its owner alone.
pub const SECRETS_FILE: &str = "secrets.json";

/// Takes every secret out of a datasource's settings, at any depth, and
/// returns them in the shape they were in.
fn take_secrets(settings: &mut serde_json::Map<String, Value>) -> serde_json::Map<String, Value> {
    let mut taken = serde_json::Map::new();
    let keys: Vec<String> = settings.keys().cloned().collect();
    for key in keys {
        if is_secret(&key) {
            if let Some(value) = settings.remove(&key) {
                taken.insert(key, value);
            }
        } else if let Some(Value::Object(inner)) = settings.get_mut(&key) {
            let within = take_secrets(inner);
            if !within.is_empty() {
                taken.insert(key, Value::Object(within));
            }
        }
    }
    taken
}

/// The secrets held, by path: `password`, `oauth.client_secret`.
fn secret_paths(held: &serde_json::Map<String, Value>, under: &str, out: &mut Vec<String>) {
    for (key, value) in held {
        let path = if under.is_empty() { key.clone() } else { format!("{under}.{key}") };
        match value {
            Value::Object(inner) if !is_secret(key) => secret_paths(inner, &path, out),
            _ => out.push(path),
        }
    }
}

/// What the files on disk are called under a model's directory.
pub const SNAPSHOT_FILE: &str = "snapshot.json.gz";

/// Whether a datasource setting is a secret by its name: the fields a
/// connection takes as one, and anything spelled like one. Generous on
/// purpose, since what it decides is whether a value is served.
pub fn is_secret(key: &str) -> bool {
    let key = key.to_lowercase();
    key == "oauth_client_id"
        || ["password", "passwd", "secret", "token", "private_key", "access_key", "api_key", "credential"]
            .iter()
            .any(|word| key.contains(word) && !key.ends_with("_file") && !key.ends_with("_path"))
}

/// A datasource's deployed settings as they may be served: every secret
/// left out at any depth, and named in `secrets` by its path.
fn shown(settings: &serde_json::Map<String, Value>, under: &str, secrets: &mut Vec<String>) -> Value {
    let mut out = serde_json::Map::new();
    for (key, value) in settings {
        let path = if under.is_empty() { key.clone() } else { format!("{under}.{key}") };
        if is_secret(key) {
            secrets.push(path);
        } else if let Value::Object(inner) = value {
            out.insert(key.clone(), shown(inner, &path, secrets));
        } else {
            out.insert(key.clone(), value.clone());
        }
    }
    Value::Object(out)
}

/// Takes every secret out of a datasource's settings for a copy that
/// leaves the server, at any depth: a key that [`is_secret`] goes with
/// its value, in a mapping inside a list as much as one at the top.
/// The test is the one that decides what is served, so a copy holds
/// what a caller could have read and no more.
pub fn strip_secrets(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| !is_secret(key));
            map.values_mut().for_each(strip_secrets);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_secrets),
        _ => {}
    }
}

pub const TESTS_FILE: &str = "tests.json";

impl Model {
    /// Over a snapshot alone: no values, no tests, the semantic model's own
    /// warnings.
    pub fn build(
        uid: &str,
        branch: &str,
        mut snapshot: Snapshot,
        dialects: &'static Dialects,
    ) -> std::result::Result<Model, zsql_semantic::BuildError> {
        // The secrets leave the snapshot before anything is built on it.
        let mut secrets = std::collections::BTreeMap::new();
        for d in &mut snapshot.datasources {
            let taken = take_secrets(&mut d.connection);
            if !taken.is_empty() {
                secrets.insert(d.uid.to_lowercase(), taken);
            }
        }
        let semantic = SemanticModel::build(snapshot)?;
        let warnings = semantic.warnings().to_vec();
        let mut tables_by_field: HashMap<zsql_semantic::Id, Vec<String>> = HashMap::new();
        for e in &semantic.snapshot().expressions {
            if let Some(t) = semantic.table(e.table_id) {
                tables_by_field.entry(e.field_id).or_default().push(t.uid.clone());
            }
        }
        let engine = Engine::new(semantic, |u| Planner::new(u, dialects, None));
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
            secrets,
        })
    }

    /// Over what the loader read: its warnings first, then the semantic model's.
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
        // What was kept apart, over anything a snapshot written before
        // secrets were kept apart still had in it.
        match std::fs::read_to_string(dir.join(SECRETS_FILE)) {
            Ok(text) => {
                let kept: std::collections::BTreeMap<String, serde_json::Map<String, Value>> =
                    serde_json::from_str(&text).map_err(|e| StoredError::Secrets(e.to_string()))?;
                m.secrets.extend(kept);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        m.loaded_at = std::fs::metadata(&snap_path).and_then(|m| m.modified()).map(utc_minute).unwrap_or_default();
        Ok(m)
    }

    /// Keeps the compiled model under `dir` so [`Model::from_stored`]
    /// can bring it back: the snapshot, the sampled values when there
    /// are any (and not a stale file when there are none), and the
    /// tests.
    pub fn write(&self, dir: &Path) -> std::io::Result<()> {
        self.write_with(dir, self.semantic().snapshot())?;
        self.write_secrets(dir)
    }

    /// What [`Model::write`] keeps, for a place other than the server's
    /// own disk: the same files but [`SECRETS_FILE`], which is never
    /// written, and a snapshot gone over once more for anything that
    /// [`is_secret`]. [`Model::from_stored`] reads it back as a model
    /// that plans the same and connects with nothing.
    pub fn write_without_secrets(&self, dir: &Path) -> std::io::Result<()> {
        let mut snapshot = self.semantic().snapshot().clone();
        for datasource in &mut snapshot.datasources {
            let mut settings = Value::Object(std::mem::take(&mut datasource.connection));
            strip_secrets(&mut settings);
            if let Value::Object(settings) = settings {
                datasource.connection = settings;
            }
            strip_secrets(&mut datasource.extra_query_params);
        }
        self.write_with(dir, &snapshot)
    }

    fn write_with(&self, dir: &Path, snapshot: &Snapshot) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        write_gz(&dir.join(SNAPSHOT_FILE), &serde_json::to_vec(snapshot)?)?;
        let values_path = dir.join(zsql_project::values::FILE_NAME);
        match &self.values {
            Some(v) => v.write(&values_path)?,
            None => {
                let _ = std::fs::remove_file(values_path);
            }
        }
        // ProjectTest serializes its file as a file name, so nothing of
        // the directory it was loaded from is kept.
        std::fs::write(dir.join(TESTS_FILE), serde_json::to_vec(&self.tests)?)
    }

    /// The secrets, in a file of their own that only its owner reads;
    /// no file when there are none.
    fn write_secrets(&self, dir: &Path) -> std::io::Result<()> {
        let path = dir.join(SECRETS_FILE);
        let _ = std::fs::remove_file(&path);
        if self.secrets.is_empty() {
            return Ok(());
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        options.open(&path)?.write_all(&serde_json::to_vec(&self.secrets)?)
    }

    /// The secrets a datasource was deployed with, by its uid in any
    /// case: for a reader that connects. Not for serving.
    pub fn secrets_of(&self, datasource: &str) -> Option<&serde_json::Map<String, Value>> {
        self.secrets.get(&datasource.trim().to_lowercase())
    }

    pub fn semantic(&self) -> &SemanticModel {
        self.engine.borrow_owner()
    }

    pub fn planner(&self) -> &Planner<'_> {
        self.engine.borrow_dependent()
    }

    /// Counts and warnings: what a deploy answers with, and what
    /// `GET /projects/{uid}/branches/{branch}` says.
    pub fn summary(&self) -> Value {
        let s = self.semantic().snapshot();
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
            let outcome = zsql_spec::lower(&spec, self.semantic(), &base)
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
        if req.context.is_none() && !self.semantic().policies().is_empty() {
            return Err(Error::invalid("ContextRequired", "this branch has security policies; a context is required to plan"));
        }
        let base = self.dialects.base()?;
        let (spec_value, made_spec, items) = spec_of(req.spec, req.expr.as_deref(), check)?;
        let t0 = Instant::now();
        let spec = zsql_spec::Spec::from_value(spec_value)?;
        let mut query = zsql_spec::lower(&spec, self.semantic(), &base)?;
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

    /// The physical tables of `datasource` a planned statement names,
    /// as the model spells them: what a change to one of them makes
    /// stale. Found by name in the statement, so a table whose name is
    /// part of another's is counted with it; never one too few.
    pub fn tables_read(&self, datasource_uid: &str, sql: &str) -> Vec<String> {
        let snapshot = self.semantic().snapshot();
        let sql = sql.to_lowercase();
        let mut tables: Vec<String> = snapshot
            .tables
            .iter()
            .filter(|t| snapshot.datasources.iter().any(|d| d.id == t.datasource_id && d.uid == datasource_uid))
            .filter(|t| sql.contains(&t.physical_name.to_lowercase()))
            .map(|t| t.physical_name.clone())
            .collect();
        tables.sort();
        tables.dedup();
        tables
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
        let query = zsql_spec::lower(&spec, self.semantic(), &base)?;
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
            return json!({ "fields": self.semantic().fields().iter().filter(visible).map(|f| describe(f, None)).collect::<Vec<_>>() });
        }
        let mut out: Vec<Value> = self
            .semantic()
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
        for s in zsql_spec::fuzzy::suggest(self.semantic(), &needle, &[FieldKind::Dimension, FieldKind::Measure]) {
            if s.score < zsql_spec::fuzzy::FLOOR / 2.0 {
                break;
            }
            if visible(&s.field) && !seen.contains(&s.field.uid) {
                out.push(describe(s.field, Some(s.score)));
            }
        }
        json!({ "fields": out })
    }

    /// The model's datasources: the key each is known by, its name,
    /// adapter and tier, and every other setting its entry was deployed
    /// with. A secret among those is never served: `connection` leaves
    /// it out and `secrets` names it, so a caller can tell which the
    /// server already holds. With `q`, the ones whose uid, name or
    /// adapter holds it.
    pub fn datasources(&self, q: &str) -> Value {
        let wanted = holds(q);
        let out: Vec<Value> = self
            .semantic()
            .snapshot()
            .datasources
            .iter()
            .filter(|d| wanted(&d.uid) || wanted(&d.name) || wanted(&d.adapter))
            .map(|d| {
                // The secrets are held apart; `shown` is the second guard
                // on what is left.
                let mut secrets = Vec::new();
                let connection = shown(&d.connection, "", &mut secrets);
                if let Some(held) = self.secrets_of(&d.uid) {
                    secret_paths(held, "", &mut secrets);
                }
                secrets.sort();
                json!({
                    "uid": d.uid, "name": d.name, "adapter": d.adapter, "tier": d.tier, "description": d.description,
                    "query_timeout": d.query_timeout, "extra_query_params": d.extra_query_params,
                    "connection": connection, "secrets": secrets,
                })
            })
            .collect();
        json!({ "datasources": out })
    }

    /// One datasource as it was deployed, by its uid in any case: for a
    /// reader that connects. Its secrets are [`Model::secrets_of`]'s.
    pub fn datasource(&self, datasource: &str) -> Option<&zsql_semantic::Datasource> {
        let want = datasource.trim().to_lowercase();
        self.semantic().snapshot().datasources.iter().find(|d| d.uid.to_lowercase() == want)
    }

    /// The model's tables. With `q`, the ones whose uid, name or
    /// physical name holds it.
    pub fn tables(&self, q: &str) -> Value {
        let u = self.semantic();
        let wanted = holds(q);
        let out: Vec<Value> = u
            .tables()
            .iter()
            .filter(|t| wanted(&t.uid) || wanted(&t.name) || wanted(&t.physical_name))
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
    /// naming them; a near miss is never taken for the user. With `q`,
    /// only the sampled values that hold it; the counts beside them
    /// still describe the whole sample.
    pub fn values_of(&self, dim: &str, q: &str) -> Result<Value> {
        let u = self.semantic();
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
                    [] => return Err(Error::not_found(format!("no dimension matches '{dim}'"))),
                    many => {
                        let uids: Vec<&str> = many.iter().map(|f| f.uid.as_str()).collect();
                        return Err(Error::invalid("Ambiguous", format!("'{dim}' matches several dimensions: {}", uids.join(", "))));
                    }
                }
            }
        };
        match self.values.as_ref().and_then(|v| v.get(&field.uid)) {
            Some(e) if q.trim().is_empty() => Ok(serde_json::to_value(e).expect("sampled values serialize")),
            Some(e) => {
                let wanted = holds(q);
                let mut e = e.clone();
                e.values.retain(|v| wanted(&v.v));
                let mut out = serde_json::to_value(&e).expect("sampled values serialize");
                // An entry with no values leaves the key out; a search
                // that found none says so with an empty list.
                out["values"] = serde_json::to_value(&e.values).expect("sampled values serialize");
                Ok(out)
            }
            None => Err(Error::not_found(format!("no sampled values for {}", field.uid))),
        }
    }
}

/// What a list's `q` keeps: text that holds it, in any case. An empty
/// `q` keeps everything, so a caller that sends none filters nothing.
pub fn holds(q: &str) -> impl Fn(&str) -> bool {
    let needle = q.trim().to_lowercase();
    move |text: &str| needle.is_empty() || text.to_lowercase().contains(&needle)
}

fn write_gz(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    enc.write_all(bytes)?;
    enc.finish()?;
    Ok(())
}

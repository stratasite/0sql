//! Golden cases: a query spec, the snapshot it planned against, the
//! security context, and the SQL (or error) strata's Rails planner
//! produced for it. `harness/import_goldens.py` brings them over from a
//! roundhouse/strata-rs capture; this crate scores the planner on them.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use zsql_dialect::Dialects;
use zsql_planner::Planner;
use zsql_semantic::Path as JoinPath;
use zsql_semantic::{Snapshot, Universe};
use zsql_spec::SecurityContext;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// `TestClass#test_name#n`, with `@dev` for the replay on a mutated branch.
    pub id: String,
    pub variant: Variant,
    /// Hash of the snapshot in `universes/`.
    pub universe: String,
    #[serde(default)]
    pub context: Option<SecurityContext>,
    pub spec: Value,
    pub expected: Expected,
    /// Dev cases: what the mutated branch changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutations: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Variant {
    Main,
    Dev,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Expected {
    Sql { sql: String },
    Error { error: ExpectedError },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedError {
    pub class: String,
    pub message: String,
}

pub struct Fixtures {
    root: PathBuf,
    dialects: Dialects,
    universes: std::sync::Mutex<HashMap<String, Arc<Universe>>>,
    formed: std::sync::Mutex<HashMap<String, Arc<Vec<JoinPath>>>>,
}

/// Where the planner takes its join paths from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSource {
    /// The paths stored with the snapshot.
    Snapshot,
    /// Paths the formation crate computes from the join graph.
    Formed,
}

impl PathSource {
    /// `ZSQL_PATHS=snapshot` selects the stored paths; the default is formation.
    pub fn from_env() -> Self {
        match std::env::var("ZSQL_PATHS").as_deref() {
            Ok("snapshot") => PathSource::Snapshot,
            _ => PathSource::Formed,
        }
    }
}

impl Fixtures {
    /// `ZSQL_GOLDEN` if set, else `fixtures/golden` in the workspace.
    pub fn default_root() -> PathBuf {
        if let Ok(p) = std::env::var("ZSQL_GOLDEN") {
            return PathBuf::from(p);
        }
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden")
    }

    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), dialects: Dialects::builtin(), universes: Default::default(), formed: Default::default() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The compiled-in dialect settings, shared by every case.
    pub fn dialects(&self) -> &Dialects {
        &self.dialects
    }

    pub fn manifest(&self) -> Result<Value> {
        let text = std::fs::read_to_string(self.root.join("manifest.json"))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Every case, grouped by the file it came from, in file order.
    pub fn cases(&self) -> Result<BTreeMap<String, Vec<Case>>> {
        let dir = self.root.join("cases");
        let mut out = BTreeMap::new();
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect();
        files.sort();
        for file in files {
            let name = file.file_stem().unwrap().to_string_lossy().to_string();
            let text = std::fs::read_to_string(&file)?;
            let mut cases = Vec::new();
            for (n, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let case: Case = serde_json::from_str(line).with_context(|| format!("{}:{}", file.display(), n + 1))?;
                cases.push(case);
            }
            out.insert(name, cases);
        }
        Ok(out)
    }

    pub fn snapshot_hashes(&self) -> Result<Vec<String>> {
        let dir = self.root.join("universes");
        let mut hashes: Vec<String> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .filter_map(|n| n.strip_suffix(".json.gz").map(str::to_string))
            .collect();
        hashes.sort();
        Ok(hashes)
    }

    pub fn snapshot(&self, hash: &str) -> Result<Snapshot> {
        let path = self.root.join("universes").join(format!("{hash}.json.gz"));
        Snapshot::from_path(&path).with_context(|| format!("loading {}", path.display()))
    }

    /// The built universe for a snapshot hash, shared across cases.
    pub fn universe(&self, hash: &str) -> Result<Arc<Universe>> {
        if let Some(u) = self.universes.lock().unwrap().get(hash) {
            return Ok(u.clone());
        }
        let u = Arc::new(Universe::build(self.snapshot(hash)?).with_context(|| format!("building universe {hash}"))?);
        self.universes.lock().unwrap().insert(hash.to_string(), u.clone());
        Ok(u)
    }

    /// Paths formed from the snapshot's join graph, shared across cases.
    pub fn formed_paths(&self, hash: &str) -> Result<Arc<Vec<JoinPath>>> {
        if let Some(p) = self.formed.lock().unwrap().get(hash) {
            return Ok(p.clone());
        }
        let u = self.universe(hash)?;
        let p = Arc::new(zsql_formation::form(&u).paths);
        self.formed.lock().unwrap().insert(hash.to_string(), p.clone());
        Ok(p)
    }

    /// The universe and, for `Formed`, the paths a planner needs.
    pub fn planner_for(&self, hash: &str, source: PathSource) -> Result<(Arc<Universe>, Option<Arc<Vec<JoinPath>>>)> {
        let u = self.universe(hash)?;
        let p = match source {
            PathSource::Formed => Some(self.formed_paths(hash)?),
            PathSource::Snapshot => None,
        };
        Ok((u, p))
    }
}

// ------------------------------------------------------------- comparison

/// CTE names carry a hash of node identity, which this planner derives from
/// table uids where Rails uses join row ids. Parity is checked with every
/// such name replaced by its order of first appearance.
pub fn normalize_cte_names(sql: &str) -> String {
    static CTE_NAME: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\b(ag|qr|mg|seg)[0-9a-f]{32}\b").unwrap());
    let re = &*CTE_NAME;
    let mut names: Vec<String> = Vec::new();
    re.replace_all(sql.trim(), |c: &regex::Captures| {
        let name = c[0].to_string();
        let idx = match names.iter().position(|n| *n == name) {
            Some(i) => i,
            None => {
                names.push(name);
                names.len() - 1
            }
        };
        format!("{}#{}", &c[1], idx)
    })
    .into_owned()
}

/// Structural equality for statements whose nodes came out in a different
/// order. CTEs are named by their content, node aliases by the CTE they
/// bind to, symmetric constructs (COALESCE arguments, equality operands,
/// AND-ed conditions) are sorted, and lines compare as sets. A swapped
/// LEFT JOIN or a changed expression still differs.
pub fn structural_form(sql: &str) -> String {
    let text = sql.trim();
    // Split "WITH a AS (\nbody\n), b AS (\nbody\n)\nfinal" into CTEs and the final statement.
    let (ctes, last): (Vec<(String, String)>, String) = if let Some(rest) = text.strip_prefix("WITH ") {
        let mut ctes = Vec::new();
        let mut cur = rest;
        while let Some(pos) = cur.find(" AS (\n") {
            let name = cur[..pos].trim_start_matches(", ").to_string();
            let body_start = pos + " AS (\n".len();
            let Some(end) = cur[body_start..].find("\n)") else { break };
            let body = cur[body_start..body_start + end].to_string();
            ctes.push((name, body));
            cur = &cur[body_start + end + 2..];
            if !cur.starts_with(", ") {
                break;
            }
        }
        (ctes, cur.trim_start_matches('\n').to_string())
    } else {
        (vec![], text.to_string())
    };
    let mut canonical: HashMap<String, String> = HashMap::new();
    let mut out: Vec<String> = Vec::new();
    for (name, body) in &ctes {
        let form = canonical_lines(body, &canonical);
        let cname = format!("cte<{:x}>", fnv(&form));
        canonical.insert(name.clone(), cname.clone());
        out.push(format!("{cname}:\n{form}"));
    }
    out.sort();
    out.push(format!("final:\n{}", canonical_lines(&last, &canonical)));
    out.join("\n\n")
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn canonical_lines(body: &str, canonical: &HashMap<String, String>) -> String {
    static CTE_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?:[A-Za-z0-9_]+\.)?\b(?:ag|qr|mg|seg)[0-9a-f]{32}\b").unwrap());
    static BIND_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"((?:[A-Za-z0-9_]+\.)?\b(?:ag|qr|mg|seg)[0-9a-f]{32}|cte<[0-9a-f]+>) (?:AS )?([AQM]\d+)\b").unwrap()
    });
    let (cte_re, bind_re) = (&*CTE_RE, &*BIND_RE);
    let mut text =
        cte_re.replace_all(body, |c: &regex::Captures| canonical.get(&c[0]).cloned().unwrap_or_else(|| c[0].to_string())).into_owned();
    // Node aliases become the CTE they bind to.
    let mut aliases: Vec<(String, String)> = Vec::new();
    for c in bind_re.captures_iter(&text) {
        aliases.push((c[2].to_string(), format!("@{}", &c[1])));
    }
    for (a, c) in &aliases {
        let re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(a))).unwrap();
        text = re.replace_all(&text, c.as_str()).into_owned();
    }
    // A FROM over nodes joined only by full or cross joins is an unordered set.
    let raw: Vec<String> = text.lines().map(|l| l.trim().to_string()).collect();
    let mut rewritten: Vec<String> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == "FROM" {
            let mut j = i + 1;
            while j < raw.len() && !matches!(raw[j].as_str(), "WHERE" | "GROUP BY" | "HAVING" | "ORDER BY") && !raw[j].starts_with("LIMIT")
            {
                j += 1;
            }
            let block = &raw[i + 1..j];
            let node_line =
                |l: &str| l.contains(" @cte<") || l.contains(" @ag") || l.contains(" @qr") || l.contains(" @mg") || l.contains(" @seg");
            let joins: Vec<&String> = block.iter().filter(|l| l.contains(" JOIN ") && node_line(l)).collect();
            let symmetric = !joins.is_empty() && joins.iter().all(|l| l.starts_with("FULL OUTER JOIN ") || l.starts_with("CROSS JOIN "));
            rewritten.push("FROM".into());
            // In a symmetric block every member joins on the same key
            // columns, incrementally through COALESCE of the ones placed
            // before it; the constraint set is one line per key column.
            let mut keys: BTreeMap<String, std::collections::BTreeSet<String>> = Default::default();
            static KEY_RE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"(@[^\s.,()]+)\.([A-Za-z0-9_]+)").unwrap());
            let key_re = &*KEY_RE;
            for l in block {
                if symmetric && node_line(l) && !l.starts_with("ON ") && !l.starts_with("AND ") {
                    let member = l.trim_start_matches("FULL OUTER JOIN ").trim_start_matches("CROSS JOIN ").to_string();
                    rewritten.push(format!("MEMBER {member}"));
                } else if symmetric && (l.starts_with("ON ") || l.starts_with("AND ")) {
                    for c in key_re.captures_iter(l) {
                        keys.entry(c[2].to_string()).or_default().insert(c[1].to_string());
                    }
                } else {
                    rewritten.push(l.clone());
                }
            }
            for (col, members) in keys {
                rewritten.push(format!("KEYJOIN {col}: {}", members.into_iter().collect::<Vec<_>>().join(", ")));
            }
            i = j;
        } else {
            rewritten.push(raw[i].clone());
            i += 1;
        }
    }
    let mut lines: Vec<String> = rewritten
        .into_iter()
        .map(|l| {
            let mut l = l.trim().to_string();
            l = sort_call_args(&l, "COALESCE(");
            if let Some(rest) = l.strip_prefix("ON ") {
                l = format!("ON {}", sort_conditions(rest));
            } else if let Some(rest) = l.strip_prefix("AND ") {
                l = format!("AND {}", sort_conditions(rest));
            }
            l
        })
        .filter(|l| !l.is_empty())
        .collect();
    lines.sort();
    lines.join("\n")
}

fn sort_conditions(s: &str) -> String {
    let mut parts: Vec<String> = s
        .split(" AND ")
        .map(|c| match c.split_once(" = ") {
            Some((a, b)) => {
                let mut ops = [a.trim(), b.trim()];
                ops.sort();
                format!("{} = {}", ops[0], ops[1])
            }
            None => c.trim().to_string(),
        })
        .collect();
    parts.sort();
    parts.join(" AND ")
}

fn sort_call_args(line: &str, prefix: &str) -> String {
    let Some(start) = line.find(prefix) else { return line.to_string() };
    let args_start = start + prefix.len();
    let mut depth = 0;
    let mut end = None;
    for (i, ch) in line[args_start..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' if depth == 0 => {
                end = Some(args_start + i);
                break;
            }
            ')' => depth -= 1,
            _ => {}
        }
    }
    let Some(end) = end else { return line.to_string() };
    let mut args: Vec<&str> = line[args_start..end].split(", ").collect();
    args.sort();
    format!("{}{}{}", &line[..args_start], args.join(", "), &line[end..])
}

// ---------------------------------------------------------------- scoring

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    /// Identical SQL once CTE names are normalised.
    PassRenamed,
    /// Structurally identical: same nodes and lines, nodes ordered differently.
    PassReordered,
    /// Planner produced SQL that differs from the golden.
    SqlMismatch {
        got: String,
    },
    /// Planner failed where SQL was expected, or with the wrong class.
    WrongError {
        got: String,
    },
    /// Planner produced SQL where an error was expected.
    NoError {
        got: String,
    },
    /// Planner has not reached this feature yet.
    Unimplemented(String),
}

impl Outcome {
    pub fn passed(&self) -> bool {
        matches!(self, Outcome::Pass | Outcome::PassRenamed | Outcome::PassReordered)
    }
}

/// Plans the case: its spec lowered against the universe, then planned
/// with its security context. An error comes back with the class name a
/// golden expects.
pub fn plan_case(planner: &Planner<'_>, dialects: &Dialects, case: &Case) -> std::result::Result<zsql_planner::Plan, (String, String)> {
    let universe = planner.universe();
    let base = dialects.base().map_err(|e| ("Unimplemented".to_string(), e.to_string()))?;
    zsql_spec::Spec::from_value(case.spec.clone())
        .and_then(|spec| zsql_spec::lower(&spec, universe, &base))
        .map_err(|e| (e.error_class().to_string(), e.to_string()))
        .and_then(|query| planner.plan_query(query, case.context.as_ref()).map_err(|e| (e.error_class().to_string(), e.to_string())))
}

pub fn run_case(fixtures: &Fixtures, case: &Case, source: PathSource) -> Result<Outcome> {
    let (universe, paths) = fixtures.planner_for(&case.universe, source)?;
    let planner = Planner::new(&universe, fixtures.dialects(), paths.as_deref().map(Vec::as_slice));
    run_case_on(&planner, fixtures.dialects(), case)
}

/// Scores one case on a planner already built for its universe.
/// The Rails class each 0sql error class was captured under. The goldens
/// record what Rails answered, down to the exception name, and are not
/// rewritten when we rename ours — parity is about which failure happens,
/// not about what the two codebases call it. A name we do not translate
/// has to match outright.
fn rails_name(class: &str) -> &str {
    match class {
        "FieldNotFound" => "Semantic::NotFound",
        "FieldAmbiguous" => "Semantic::Ambiguous",
        "InvalidSpec" => "Query::Spec::InvalidSpecError",
        "InvalidValue" => "ActiveRecord::RecordInvalid",
        "Unresolvable" => "Planner::ResolutionError",
        "SegmentDatasource" => "Planner::SegmentDatasourceError",
        "SecurityPolicy" => "Planner::SecurityPolicyError",
        other => other,
    }
}

/// True when our class and the golden's name the same failure.
fn same_failure(ours: &str, golden: &str) -> bool {
    ours == golden || rails_name(ours) == golden
}

pub fn run_case_on(planner: &Planner<'_>, dialects: &Dialects, case: &Case) -> Result<Outcome> {
    Ok(match (&case.expected, plan_case(planner, dialects, case)) {
        (_, Err((class, msg))) if class == "Unimplemented" => Outcome::Unimplemented(msg),
        (Expected::Sql { sql }, Ok(p)) => {
            if p.sql.trim() == sql.trim() {
                Outcome::Pass
            } else if normalize_cte_names(&p.sql) == normalize_cte_names(sql) {
                Outcome::PassRenamed
            } else if structural_form(&p.sql) == structural_form(sql) {
                Outcome::PassReordered
            } else {
                Outcome::SqlMismatch { got: p.sql }
            }
        }
        (Expected::Sql { .. }, Err((class, msg))) => Outcome::WrongError { got: format!("{class}: {msg}") },
        (Expected::Error { error }, Err((class, msg))) => {
            if same_failure(&class, &error.class) {
                Outcome::Pass
            } else {
                Outcome::WrongError { got: format!("{class}: {msg}") }
            }
        }
        (Expected::Error { .. }, Ok(p)) => Outcome::NoError { got: p.sql },
    })
}

#[derive(Debug, Default)]
pub struct FileScore {
    pub total: usize,
    pub passed: usize,
    /// Passes that needed CTE-name normalisation.
    pub renamed: usize,
    /// Passes that needed the structural comparison.
    pub reordered: usize,
    pub unimplemented: usize,
    pub failed: Vec<(String, Outcome)>,
}

/// Scores every case whose id contains `filter` (all when empty).
pub fn score(fixtures: &Fixtures, filter: &str, source: PathSource) -> Result<BTreeMap<String, FileScore>> {
    let dialects = fixtures.dialects();
    let mut report = BTreeMap::new();
    for (file, cases) in fixtures.cases()? {
        let mut fs = FileScore::default();
        // One planner per universe the file's cases use.
        let mut hashes: Vec<&str> = cases.iter().map(|c| c.universe.as_str()).collect();
        hashes.sort();
        hashes.dedup();
        let universes: Vec<(Arc<Universe>, Option<Arc<Vec<JoinPath>>>)> =
            hashes.iter().map(|h| fixtures.planner_for(h, source)).collect::<Result<_>>()?;
        let planners: HashMap<&str, Planner<'_>> = hashes
            .iter()
            .zip(universes.iter())
            .map(|(h, (u, p))| (*h, Planner::new(u, dialects, p.as_deref().map(Vec::as_slice))))
            .collect();
        for case in cases.iter().filter(|c| c.id.contains(filter)) {
            fs.total += 1;
            match run_case_on(&planners[case.universe.as_str()], dialects, case)? {
                Outcome::Pass => fs.passed += 1,
                Outcome::PassRenamed => {
                    fs.passed += 1;
                    fs.renamed += 1;
                }
                Outcome::PassReordered => {
                    fs.passed += 1;
                    fs.reordered += 1;
                }
                Outcome::Unimplemented(_) => fs.unimplemented += 1,
                other => fs.failed.push((case.id.clone(), other)),
            }
        }
        if fs.total > 0 {
            report.insert(file, fs);
        }
    }
    Ok(report)
}

pub fn print_scoreboard(report: &BTreeMap<String, FileScore>) -> (usize, usize) {
    let (mut total, mut passed) = (0, 0);
    println!("{:<36} {:>6} {:>6} {:>8} {:>10} {:>8} {:>6}", "file", "cases", "pass", "renamed", "reordered", "unimpl", "fail");
    for (file, fs) in report {
        println!(
            "{:<36} {:>6} {:>6} {:>8} {:>10} {:>8} {:>6}",
            file,
            fs.total,
            fs.passed,
            fs.renamed,
            fs.reordered,
            fs.unimplemented,
            fs.failed.len()
        );
        total += fs.total;
        passed += fs.passed;
    }
    println!("{:<36} {:>6} {:>6}", "total", total, passed);
    (total, passed)
}

// -------------------------------------------------------------- formation

/// How formed paths compare with the stored ones: per (root, field), the
/// cheapest stored route against the formed route.
#[derive(Debug, Default)]
pub struct PathReport {
    pub pairs: usize,
    pub matched: usize,
    pub mismatches: Vec<String>,
    pub only_stored: usize,
    pub only_formed: usize,
}

impl PathReport {
    pub fn summary(&self) -> String {
        format!(
            "root/field pairs: {}, matched: {}, mismatched: {}, only in stored: {}, only in formed: {}",
            self.pairs,
            self.matched,
            self.mismatches.len(),
            self.only_stored,
            self.only_formed
        )
    }
}

pub fn compare_paths(universe: &Universe, stored: &[JoinPath], formed: &[JoinPath]) -> PathReport {
    let cheapest = |paths: &'_ [JoinPath]| -> BTreeMap<(i64, i64), JoinPath> {
        let mut m: BTreeMap<(i64, i64), JoinPath> = BTreeMap::new();
        for p in paths {
            let key = (p.root_table_id, p.field_id);
            let better = match m.get(&key) {
                Some(e) => (p.cost, universe.path_signature(p)) < (e.cost, universe.path_signature(e)),
                None => true,
            };
            if better {
                m.insert(key, p.clone());
            }
        }
        m
    };
    let s = cheapest(stored);
    let f = cheapest(formed);
    let name = |id: i64| universe.table(id).map(|t| t.uid.clone()).unwrap_or_else(|| id.to_string());
    let fname = |id: i64| universe.field(id).map(|t| t.uid.clone()).unwrap_or_else(|| id.to_string());
    let mut report = PathReport::default();
    for (key, sp) in &s {
        report.pairs += 1;
        match f.get(key) {
            None => {
                report.only_stored += 1;
                report.mismatches.push(format!("{} -> {}: stored via {:?}, not formed", name(key.0), fname(key.1), sp.joins));
            }
            Some(fp) => {
                let mut sj = sp.joins.clone();
                sj.sort();
                let mut fj = fp.joins.clone();
                fj.sort();
                if sp.target_table_id == fp.target_table_id && sj == fj && sp.cost == fp.cost {
                    report.matched += 1;
                } else {
                    report.mismatches.push(format!(
                        "{} -> {}: stored target {} joins {:?} cost {}, formed target {} joins {:?} cost {}",
                        name(key.0),
                        fname(key.1),
                        name(sp.target_table_id),
                        sp.joins,
                        sp.cost,
                        name(fp.target_table_id),
                        fp.joins,
                        fp.cost
                    ));
                }
            }
        }
    }
    report.only_formed = f.keys().filter(|k| !s.contains_key(k)).count();
    report
}

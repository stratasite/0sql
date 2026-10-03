//! A strata CLI project directory, loaded into a [`Snapshot`] the planner
//! can use, without a server. The layout is what `strata init` produces
//! and `strata deploy` uploads:
//!
//! ```text
//! project.yml           name, uid, production_branch
//! datasources.yml       one warehouse per key: name, adapter, tier, ...
//! models/**/tbl.*.yml   a table with its dimensions and measures
//! models/**/rel.*.yml   joins between a datasource's tables
//! security.yml          row-level security policies
//! tests/*.yml           planner assertions (see [`ProjectTest`])
//! ```
//!
//! Loading follows the Rails deployer's handlers (`Deployer::Handlers::*`)
//! and its formation cycle: tables and fields, then joins, then paths, then
//! what needs paths (snapshot dates, partitions, exclusion and inclusion
//! rules, expression integrity), then policies. Ids are sequential; uids
//! are derived from names the way FriendlyId does on the server, so a
//! project deployed to a server and loaded here name the same things.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value as Json;
use serde_yaml::Value as Yaml;
use zsql_dialect::Dialects;
use zsql_spec::expr::ParsedExpression;
use zsql_semantic::*;

pub mod values;

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("{0}: {1}")]
    Io(String, std::io::Error),
    #[error("{0}: {1}")]
    Yaml(String, serde_yaml::Error),
    /// A definition the deployer would refuse, with the file it is in.
    #[error("Error in {file}: \n\t{message}")]
    Invalid { file: String, message: String },
    #[error("{0}")]
    Dialect(#[from] zsql_dialect::DialectError),
    #[error("{0}")]
    Universe(#[from] BuildError),
}

type Result<T> = std::result::Result<T, ProjectError>;

fn invalid(file: &Path, message: impl Into<String>) -> ProjectError {
    ProjectError::Invalid { file: file.display().to_string(), message: message.into() }
}

/// A loaded project: the snapshot, with paths formed, and what the loader
/// noticed but did not refuse.
#[derive(Debug)]
pub struct Loaded {
    pub snapshot: Snapshot,
    pub warnings: Vec<String>,
    pub tests: Vec<ProjectTest>,
    /// Sampled dimension values (`values.jsonl.gz`), when the project has them.
    pub values: Option<values::ValueCatalog>,
}

/// One `tests/*.yml` entry: fields to project by name and what the SQL
/// must equal or match.
#[derive(Debug, Clone)]
pub struct ProjectTest {
    pub file: PathBuf,
    pub name: String,
    pub projections: Vec<String>,
    pub assert_sql: Option<String>,
    pub assert_regex: Option<String>,
}

/// Loads the project at `dir`. `dialects` supplies each datasource's
/// reserved keywords, aggregate functions and identifier quote, which the
/// expression parser needs to find the columns an expression reads.
pub fn load(dir: &Path, dialects: &Dialects) -> Result<Loaded> {
    let mut b = Builder::new(dir, dialects);
    b.project()?;
    b.datasources()?;
    b.tables()?;
    b.relationships()?;
    b.form()?;
    b.post_universe()?;
    b.security()?;
    b.tests()?;
    Ok(b.finish())
}

// ------------------------------------------------------------------ YAML

fn read_yaml(path: &Path) -> Result<Yaml> {
    let text = std::fs::read_to_string(path).map_err(|e| ProjectError::Io(path.display().to_string(), e))?;
    let v: Yaml = serde_yaml::from_str(&text).map_err(|e| ProjectError::Yaml(path.display().to_string(), e))?;
    Ok(v)
}

fn as_map(v: &Yaml) -> Option<&serde_yaml::Mapping> {
    v.as_mapping()
}

fn get<'a>(m: &'a serde_yaml::Mapping, key: &str) -> Option<&'a Yaml> {
    m.get(Yaml::String(key.to_string())).filter(|v| !v.is_null())
}

fn text(v: &Yaml) -> Option<String> {
    match v {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Number(n) => Some(n.to_string()),
        Yaml::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn str_of(m: &serde_yaml::Mapping, key: &str) -> Option<String> {
    get(m, key).and_then(text).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn bool_of(m: &serde_yaml::Mapping, key: &str) -> Option<bool> {
    match get(m, key)? {
        Yaml::Bool(b) => Some(*b),
        Yaml::String(s) => match s.trim().to_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        },
        Yaml::Number(n) => n.as_i64().map(|n| n != 0),
        _ => None,
    }
}

fn int_of(m: &serde_yaml::Mapping, key: &str) -> Option<i64> {
    match get(m, key)? {
        Yaml::Number(n) => n.as_i64(),
        Yaml::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn strs_of(m: &serde_yaml::Mapping, key: &str) -> Vec<String> {
    match get(m, key) {
        Some(Yaml::Sequence(items)) => items.iter().filter_map(text).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Some(v) => text(v).map(|s| vec![s.trim().to_string()]).unwrap_or_default(),
        None => vec![],
    }
}

fn yaml_to_json(v: &Yaml) -> Json {
    serde_json::to_value(v).unwrap_or(Json::Null)
}

// ------------------------------------------------------------------ uids

/// ActiveSupport `parameterize`: ASCII letters, digits, `_` and `-`
/// survive, everything else becomes one `-`, and the ends are trimmed.
pub fn parameterize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut dash = false;
    for ch in s.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
            dash = ch == '-';
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// FriendlyId with candidates: the first free slug wins; when every
/// candidate is taken a counter is appended (Rails appends a UUID).
struct Slugs {
    used: HashSet<String>,
}

impl Slugs {
    fn new() -> Self {
        Slugs { used: HashSet::new() }
    }

    fn take(&mut self, candidates: &[String]) -> String {
        for c in candidates {
            let slug = parameterize(c);
            if !slug.is_empty() && self.used.insert(slug.clone()) {
                return slug;
            }
        }
        let base = candidates.first().map(|c| parameterize(c)).filter(|s| !s.is_empty()).unwrap_or_else(|| "item".into());
        let mut n = 2;
        loop {
            let slug = format!("{base}-{n}");
            if self.used.insert(slug.clone()) {
                return slug;
            }
            n += 1;
        }
    }
}

// --------------------------------------------------------------- builder

struct PendingExpression {
    expression_index: usize,
    file: PathBuf,
    field_name: String,
    measure_refs: Vec<String>,
    dimension_refs: Vec<String>,
    /// `[Name]` with no `@d`/`@m`. Resolved against both kinds once every
    /// file is loaded, since the field it names may live in another one.
    bare_refs: Vec<String>,
}

struct PendingTable {
    table_index: usize,
    file: PathBuf,
    snapshot: Option<String>,
    partitions: Vec<serde_yaml::Mapping>,
}

struct PendingRule {
    field_index: usize,
    file: PathBuf,
}

struct Builder<'d> {
    dir: PathBuf,
    dialects: &'d Dialects,
    warnings: Vec<String>,
    project: Option<ProjectRef>,
    production_branch: String,
    datasources: Vec<Datasource>,
    tables: Vec<Table>,
    fields: Vec<Field>,
    expressions: Vec<Expression>,
    join_defs: Vec<JoinDef>,
    partitions: Vec<Partition>,
    policies: Vec<SecurityPolicy>,
    paths: Vec<zsql_semantic::Path>,
    tests: Vec<ProjectTest>,
    field_slugs: Slugs,
    table_slugs: Slugs,
    policy_slugs: Slugs,
    /// (kind, lower-cased name) -> field index.
    field_index: HashMap<(FieldKind, String), usize>,
    pending_expressions: Vec<PendingExpression>,
    pending_tables: Vec<PendingTable>,
    pending_rules: Vec<PendingRule>,
}

const TABLE_KEYS: &[&str] = &["datasource", "name", "physical_name", "cost", "description", "tags", "metadata", "snapshot", "snapshot_date", "partition", "partitions", "imports", "fields"];
const FIELD_KEYS: &[&str] = &[
    "type", "name", "description", "hidden", "grains", "data_type", "display_type", "format", "secure", "disable_listing", "disable_value_listing", "value_list_size", "snapshot",
    "exclusion_type", "exclusions", "inclusions", "extended_blend_group", "synonyms", "tags", "expression", "values",
];
const EXPRESSION_KEYS: &[&str] = &["sql", "array", "lookup", "primary_key"];
const JOIN_KEYS: &[&str] = &["left", "right", "sql", "cardinality", "join", "allow_measure_expansion", "description"];
const DATE_GRAINS: &[&str] = &["raw", "millisecond", "second", "minute", "hour", "day", "month", "week", "quarter", "year"];

impl<'d> Builder<'d> {
    fn new(dir: &Path, dialects: &'d Dialects) -> Self {
        Builder {
            dir: dir.to_path_buf(),
            dialects,
            warnings: vec![],
            project: None,
            production_branch: "main".into(),
            datasources: vec![],
            tables: vec![],
            fields: vec![],
            expressions: vec![],
            join_defs: vec![],
            partitions: vec![],
            policies: vec![],
            paths: vec![],
            tests: vec![],
            field_slugs: Slugs::new(),
            table_slugs: Slugs::new(),
            policy_slugs: Slugs::new(),
            field_index: HashMap::new(),
            pending_expressions: vec![],
            pending_tables: vec![],
            pending_rules: vec![],
        }
    }

    fn warn(&mut self, file: &Path, message: impl Into<String>) {
        self.warnings.push(format!("{}: {}", self.relative(file), message.into()));
    }

    fn relative(&self, file: &Path) -> String {
        file.strip_prefix(&self.dir).unwrap_or(file).display().to_string()
    }

    /// The `models/...` path Rails records as a table's or join's source.
    fn source_of(&self, file: &Path) -> Option<String> {
        let rel = self.relative(file);
        rel.find("models/").map(|i| rel[i..].to_string())
    }

    fn unknown_keys(&mut self, file: &Path, what: &str, m: &serde_yaml::Mapping, known: &[&str]) {
        for k in m.keys() {
            if let Some(k) = k.as_str() {
                if !known.contains(&k) {
                    self.warn(file, format!("{what}: unknown key '{k}' is ignored"));
                }
            }
        }
    }

    fn files(&self, pattern_dir: &str, prefix: &str) -> Result<Vec<PathBuf>> {
        let root = self.dir.join(pattern_dir);
        let mut out = Vec::new();
        if root.is_dir() {
            walk(&root, &mut out).map_err(|e| ProjectError::Io(root.display().to_string(), e))?;
        }
        out.retain(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.starts_with(prefix) && (name.ends_with(".yml") || name.ends_with(".yaml"))
        });
        out.sort();
        Ok(out)
    }

    // ---- project.yml

    fn project(&mut self) -> Result<()> {
        let file = self.dir.join("project.yml");
        let (name, uid, branch) = if file.exists() {
            let v = read_yaml(&file)?;
            let m = as_map(&v).ok_or_else(|| invalid(&file, "project.yml must be a mapping"))?;
            let name = str_of(m, "name").ok_or_else(|| invalid(&file, "Cannot create project: name is required in project.yml"))?;
            let uid = str_of(m, "uid").map(|u| u.to_lowercase()).unwrap_or_else(|| parameterize(&name));
            (name, uid, str_of(m, "production_branch").unwrap_or_else(|| "main".into()))
        } else {
            let name = self.dir.file_name().and_then(|n| n.to_str()).unwrap_or("project").to_string();
            self.warnings.push("project.yml not found; the directory name stands in for the project".into());
            (name.clone(), parameterize(&name), "main".into())
        };
        self.project = Some(ProjectRef { uid, name });
        self.production_branch = branch;
        Ok(())
    }

    // ---- datasources.yml

    fn datasources(&mut self) -> Result<()> {
        let file = self.dir.join("datasources.yml");
        if !file.exists() {
            return Err(invalid(&file, "datasources.yml is required"));
        }
        let v = read_yaml(&file)?;
        let m = as_map(&v).ok_or_else(|| invalid(&file, "datasources.yml must map each datasource key to its settings"))?;
        for (key, def) in m {
            let Some(uid) = key.as_str() else { continue };
            let uid = uid.trim().to_lowercase();
            let Some(d) = as_map(def) else {
                return Err(invalid(&file, format!("datasource {uid} must be a mapping")));
            };
            let name = str_of(d, "name").unwrap_or_else(|| uid.clone());
            let adapter = str_of(d, "adapter").ok_or_else(|| invalid(&file, format!("Datasource errors: {name} needs an adapter")))?.to_lowercase();
            self.dialects.get(&adapter).map_err(|_| invalid(&file, format!("Datasource errors: adapter '{adapter}' is not supported")))?;
            let tier = match str_of(d, "tier").map(|t| t.to_lowercase()).as_deref() {
                None => None,
                Some("hot") => Some(Tier::Hot),
                Some("warm") => Some(Tier::Warm),
                Some("cold") => Some(Tier::Cold),
                Some(other) => return Err(invalid(&file, format!("Datasource errors: tier '{other}' should be hot, warm or cold ({name})"))),
            };
            if self.datasources.iter().any(|x| x.uid == uid) {
                return Err(invalid(&file, format!("datasource key {uid} is repeated")));
            }
            let id = self.datasources.len() as Id + 1;
            self.datasources.push(Datasource {
                id,
                uid,
                name,
                adapter,
                tier,
                description: str_of(d, "description"),
                extra_query_params: get(d, "extra_query_params").map(yaml_to_json).unwrap_or(Json::Object(Default::default())),
                query_timeout: int_of(d, "query_timeout"),
            });
        }
        if self.datasources.is_empty() {
            return Err(invalid(&file, "datasources.yml defines no datasource"));
        }
        Ok(())
    }

    fn datasource_for(&self, file: &Path, m: &serde_yaml::Mapping, what: &str) -> Result<usize> {
        let val = str_of(m, "datasource").ok_or_else(|| invalid(file, format!("datasource key is required in {what} file.")))?.to_lowercase();
        self.datasources
            .iter()
            .position(|d| d.uid == val || d.name.to_lowercase() == val)
            .ok_or_else(|| invalid(file, format!("datasource with uid or name {val} not found in branch.")))
    }

    // ---- models/**/tbl.*.yml

    fn tables(&mut self) -> Result<()> {
        let files = self.files("models", "tbl.")?;
        for file in files {
            let def = self.resolve_imports(&file, &mut Vec::new())?;
            self.table(&file, &def)?;
        }
        Ok(())
    }

    /// The CLI's `YamlImportResolver`: imported files' fields first, local
    /// fields override them by name (a local blank keeps the import's
    /// value), and with imports the fields sort by name.
    fn resolve_imports(&mut self, file: &Path, visiting: &mut Vec<PathBuf>) -> Result<serde_yaml::Mapping> {
        let abs = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
        if visiting.contains(&abs) {
            let cycle: Vec<String> = visiting.iter().chain(std::iter::once(&abs)).map(|p| p.file_name().unwrap_or_default().to_string_lossy().to_string()).collect();
            return Err(invalid(file, format!("Circular import detected: {}", cycle.join(" -> "))));
        }
        visiting.push(abs);
        let v = read_yaml(file)?;
        let mut m = as_map(&v).cloned().ok_or_else(|| invalid(file, "Empty file detected."))?;
        let imports = strs_of(&m, "imports");
        m.remove(Yaml::String("imports".into()));
        if imports.is_empty() {
            visiting.pop();
            return Ok(m);
        }
        let mut imported: Vec<serde_yaml::Mapping> = Vec::new();
        for import in &imports {
            let base = file.parent().unwrap_or(&self.dir).join(import);
            let path = if base.exists() { base } else { self.dir.join(import) };
            if !path.exists() {
                return Err(invalid(file, format!("Import not found: {import}")));
            }
            let sub = self.resolve_imports(&path, visiting)?;
            if let Some(Yaml::Sequence(fields)) = get(&sub, "fields") {
                imported.extend(fields.iter().filter_map(|f| as_map(f).cloned()));
            }
        }
        visiting.pop();
        let local: Vec<serde_yaml::Mapping> = match get(&m, "fields") {
            Some(Yaml::Sequence(fields)) => fields.iter().filter_map(|f| as_map(f).cloned()).collect(),
            _ => vec![],
        };
        let mut merged: BTreeMap<String, serde_yaml::Mapping> = BTreeMap::new();
        for f in imported {
            if let Some(name) = str_of(&f, "name") {
                merged.insert(name, f);
            }
        }
        for f in local {
            let Some(name) = str_of(&f, "name") else { continue };
            match merged.get(&name).cloned() {
                Some(base) => {
                    let mut out = f.clone();
                    for (k, v) in base {
                        if k.as_str() == Some("name") {
                            continue;
                        }
                        let blank = match out.get(&k) {
                            None | Some(Yaml::Null) => true,
                            Some(Yaml::String(s)) => s.is_empty(),
                            _ => false,
                        };
                        if blank {
                            out.insert(k, v);
                        }
                    }
                    merged.insert(name, out);
                }
                None => {
                    merged.insert(name, f);
                }
            }
        }
        m.insert(Yaml::String("fields".into()), Yaml::Sequence(merged.into_values().map(Yaml::Mapping).collect()));
        Ok(m)
    }

    fn table(&mut self, file: &Path, m: &serde_yaml::Mapping) -> Result<()> {
        self.unknown_keys(file, "table", m, TABLE_KEYS);
        let ds = self.datasource_for(file, m, "table")?;
        let name = str_of(m, "name").ok_or_else(|| invalid(file, "Table errors: Name can't be blank"))?;
        let physical_name = str_of(m, "physical_name").ok_or_else(|| invalid(file, format!("Table errors: Physical name can't be blank ({name})")))?;
        if self.tables.iter().any(|t| t.datasource_id == self.datasources[ds].id && t.name.to_lowercase() == name.to_lowercase()) {
            return Err(invalid(file, format!("Table errors: Name has already been taken ({name})")));
        }
        let cost = match int_of(m, "cost") {
            Some(c) => c,
            None => {
                self.warn(file, format!("table {name} has no cost; 0 assumed"));
                0
            }
        };
        let project_uid = self.project.as_ref().map(|p| p.uid.clone()).unwrap_or_default();
        let ds_uid = self.datasources[ds].uid.clone();
        let uid = self.table_slugs.take(&[name.clone(), format!("{ds_uid}-{name}"), format!("{project_uid}-{ds_uid}-{name}")]);
        let id = self.tables.len() as Id + 1;
        self.tables.push(Table {
            id,
            uid,
            name: name.clone(),
            physical_name,
            datasource_id: self.datasources[ds].id,
            cost,
            snapshot_date_id: None,
            tags: strs_of(m, "tags"),
            source: self.source_of(file),
            description: str_of(m, "description"),
            clear_cache_on_data_update: false,
            data_updated_at: None,
        });
        let table_index = self.tables.len() - 1;

        let partitions: Vec<serde_yaml::Mapping> = match get(m, "partition").or_else(|| get(m, "partitions")) {
            Some(Yaml::Sequence(items)) => items.iter().filter_map(|p| as_map(p).cloned()).collect(),
            Some(_) => return Err(invalid(file, format!("Partition errors: partitions must be a list ({name})"))),
            None => vec![],
        };
        self.pending_tables.push(PendingTable {
            table_index,
            file: file.to_path_buf(),
            snapshot: str_of(m, "snapshot").or_else(|| str_of(m, "snapshot_date")),
            partitions,
        });

        let fields = match get(m, "fields") {
            Some(Yaml::Sequence(items)) => items.clone(),
            Some(Yaml::Null) | None => vec![],
            Some(_) => return Err(invalid(file, format!("fields must be a list ({name})"))),
        };
        for f in &fields {
            let Some(fm) = as_map(f) else {
                return Err(invalid(file, format!("each field of {name} must be a mapping")));
            };
            self.field(file, table_index, ds, fm)?;
        }
        Ok(())
    }

    fn field(&mut self, file: &Path, table_index: usize, ds: usize, fm: &serde_yaml::Mapping) -> Result<()> {
        self.unknown_keys(file, "field", fm, FIELD_KEYS);
        let name = str_of(fm, "name").ok_or_else(|| invalid(file, "Field errors: Name can't be blank"))?;
        let kind = match str_of(fm, "type").map(|t| t.to_lowercase()).as_deref() {
            Some("dimension") => FieldKind::Dimension,
            Some("measure") => FieldKind::Measure,
            _ => return Err(invalid(file, format!("Field {name} errors: type should either be a dimension or measure"))),
        };
        let data_type: DataType = match str_of(fm, "data_type") {
            Some(d) => serde_json::from_value(Json::String(d.to_lowercase())).map_err(|_| invalid(file, format!("Field {name} errors: Data type '{d}' is not included in the list")))?,
            None => return Err(invalid(file, format!("Field {name} errors: Data type can't be blank"))),
        };
        let display_type = str_of(fm, "display_type").map(|d| d.to_lowercase()).unwrap_or_else(|| "default".into());
        if !["default", "html", "url", "email", "phone_number", "image"].contains(&display_type.as_str()) {
            return Err(invalid(file, format!("Field {name} errors: Display type '{display_type}' is not included in the list")));
        }
        let grains = strs_of(fm, "grains").into_iter().map(|g| g.to_lowercase()).collect::<Vec<_>>();
        if let Some(bad) = grains.iter().find(|g| !DATE_GRAINS.contains(&g.as_str())) {
            return Err(invalid(file, format!("Field {name} errors: Grains contains invalid values: {bad}")));
        }
        let snapshot = match str_of(fm, "snapshot").map(|s| s.to_lowercase()).as_deref() {
            None => None,
            Some("ending") => Some(SnapshotKind::Ending),
            Some("beginning") => Some(SnapshotKind::Beginning),
            Some(other) => return Err(invalid(file, format!("Field {name} errors: Snapshot '{other}' is not included in the list"))),
        };
        let exclusion_type = match str_of(fm, "exclusion_type").map(|s| s.to_lowercase()).as_deref() {
            None => None,
            Some("exclude") => Some(ExclusionType::Exclude),
            Some("exclude_all_except") => Some(ExclusionType::ExcludeAllExcept),
            Some("exclude_all") => Some(ExclusionType::ExcludeAll),
            Some(other) => return Err(invalid(file, format!("Field {name} errors: Exclusion type '{other}' is not included in the list"))),
        };
        let exclusions = self.exclusions(file, &name, fm)?;
        if exclusion_type == Some(ExclusionType::ExcludeAll) && !exclusions.is_empty() {
            return Err(invalid(file, format!("Field {name} errors: Exclusions should be empty when exclusion_type is exclude_all")));
        }
        let inclusions = self.inclusion(file, &name, fm)?;
        let format = match get(fm, "format") {
            None => None,
            Some(Yaml::String(s)) => parse_format_syntax(s),
            Some(v @ Yaml::Mapping(_)) => normalize_format(yaml_to_json(v)),
            Some(_) => return Err(invalid(file, format!("Field {name} errors: format must be a shortcut string or a mapping"))),
        };

        let key = (kind, name.trim().to_lowercase());
        let field_index = match self.field_index.get(&key) {
            Some(&i) => i,
            None => {
                let clean = name.replace('%', "percent").replace('#', "num");
                let schema_type = match kind {
                    FieldKind::Dimension => "dimension",
                    FieldKind::Measure => "measure",
                };
                let uid = self.field_slugs.take(&[clean.clone(), format!("{schema_type}-{clean}")]);
                let id = self.fields.len() as Id + 1;
                self.fields.push(Field {
                    id,
                    uid,
                    name: name.clone(),
                    kind,
                    data_type: Some(data_type),
                    display_type: display_type.clone(),
                    hidden: false,
                    tags: vec![],
                    grains: vec![],
                    format: None,
                    exclusion_type: None,
                    exclusions: vec![],
                    inclusions: None,
                    extended_blend_group: None,
                    snapshot: None,
                    secure: false,
                    disable_listing: false,
                    value_list_size: 1000,
                    value_sampling: ValueSampling::Sample,
                    synonyms: vec![],
                    description: None,
                    rank: 0,
                });
                self.field_index.insert(key, self.fields.len() - 1);
                self.fields.len() - 1
            }
        };
        // Every definition of a field updates it; the last one deployed wins,
        // as on the server.
        {
            let f = &mut self.fields[field_index];
            f.name = name.clone();
            f.data_type = Some(data_type);
            f.display_type = display_type;
            f.hidden = bool_of(fm, "hidden").unwrap_or(false);
            f.tags = strs_of(fm, "tags");
            f.grains = grains;
            f.format = format;
            f.exclusion_type = exclusion_type;
            f.exclusions = exclusions;
            f.inclusions = inclusions;
            f.extended_blend_group = str_of(fm, "extended_blend_group").map(|g| g.to_lowercase());
            f.snapshot = snapshot;
            f.secure = bool_of(fm, "secure").unwrap_or(false);
            f.disable_listing = bool_of(fm, "disable_listing").or_else(|| bool_of(fm, "disable_value_listing")).unwrap_or(false);
            f.value_list_size = int_of(fm, "value_list_size").unwrap_or(1000);
            f.value_sampling = match str_of(fm, "values").map(|v| v.to_lowercase()).as_deref() {
                None | Some("sample") => ValueSampling::Sample,
                Some("full") => ValueSampling::Full,
                Some("none") | Some("false") => ValueSampling::None,
                Some(other) => return Err(invalid(file, format!("Field {name} errors: values '{other}' should be sample, full or none"))),
            };
            f.synonyms = strs_of(fm, "synonyms");
            f.description = str_of(fm, "description");
        }
        if self.fields[field_index].exclusion_type.is_some() || self.fields[field_index].inclusions.is_some() {
            self.pending_rules.push(PendingRule { field_index, file: file.to_path_buf() });
        }

        // The expression on this table.
        let (sql, array, lookup, primary_key) = match get(fm, "expression").or_else(|| get(fm, "expresssion")) {
            Some(Yaml::String(s)) => (s.trim().to_string(), false, false, false),
            Some(Yaml::Mapping(em)) => {
                self.unknown_keys(file, &format!("expression of {name}"), em, EXPRESSION_KEYS);
                let sql = str_of(em, "sql").ok_or_else(|| invalid(file, format!("Expression errors for {name}: Sql can't be blank")))?;
                (sql, bool_of(em, "array").unwrap_or(false), bool_of(em, "lookup").unwrap_or(false), bool_of(em, "primary_key").unwrap_or(false))
            }
            Some(_) => return Err(invalid(file, "Field expression must be a string or mapping.")),
            None => return Err(invalid(file, format!("Field '{name}' is missing an expression node."))),
        };
        let table_id = self.tables[table_index].id;
        let field_id = self.fields[field_index].id;
        if self.expressions.iter().any(|e| e.table_id == table_id && e.field_id == field_id) {
            return Err(invalid(file, format!("Expression errors for {name}: Field {name} is already mapped to this table.")));
        }
        let dialect = self.dialects.get(&self.datasources[ds].adapter)?;
        let quote = dialect.get("quote").and_then(|v| v.as_str()).unwrap_or("\"@exp\"").to_string();
        let parsed = ParsedExpression::parse(&sql, &dialect.reserved_keywords(), &dialect.aggregate_functions(), &quote);
        if kind == FieldKind::Dimension && parsed.columns.is_empty() {
            return Err(invalid(file, format!("Expression errors for {name}: Column refs dimensions should reference a table column")));
        }
        if kind == FieldKind::Measure && !parsed.columns.is_empty() && !measure_has_function(&sql) {
            return Err(invalid(file, format!("Expression errors for {name}: Sql measure should have an aggregation function")));
        }
        let id = self.expressions.len() as Id + 1;
        self.expressions.push(Expression {
            id,
            table_id,
            field_id,
            sql: Some(sql),
            lookup,
            primary_key,
            array,
            column_refs: parsed.columns.clone(),
            dimension_ref_ids: vec![],
            measure_ref_ids: vec![],
        });
        if !parsed.measure_refs.is_empty() || !parsed.dimension_refs.is_empty() || !parsed.field_refs.is_empty() {
            self.pending_expressions.push(PendingExpression {
                expression_index: self.expressions.len() - 1,
                file: file.to_path_buf(),
                field_name: name,
                measure_refs: parsed.measure_refs,
                dimension_refs: parsed.dimension_refs,
                bare_refs: parsed.field_refs,
            });
        }
        Ok(())
    }

    fn exclusions(&mut self, file: &Path, field: &str, fm: &serde_yaml::Mapping) -> Result<Vec<Exclusion>> {
        let Some(list) = get(fm, "exclusions") else { return Ok(vec![]) };
        let Yaml::Sequence(items) = list else {
            return Err(invalid(file, format!("Field {field} errors: exclusions must be a list")));
        };
        let mut out = Vec::new();
        for it in items {
            let Some(em) = as_map(it) else {
                return Err(invalid(file, format!("Field {field} errors: each exclusion must be a mapping")));
            };
            let scope = match str_of(em, "type").map(|s| s.to_lowercase()).as_deref() {
                Some("dimension") => ExclusionScope::Dimension,
                Some("table") => ExclusionScope::Table,
                Some("universe") => ExclusionScope::Universe,
                _ => return Err(invalid(file, format!("Field {field} errors: Exclusion rule Type should be one of: dimension, table, universe"))),
            };
            let filter = parse_filter_mode(str_of(em, "filter")).ok_or_else(|| invalid(file, format!("Field {field} errors: Exclusion rule Filter should be one of: ignore, apply, only")))?;
            let entities = strs_of(em, "entities");
            if entities.is_empty() {
                return Err(invalid(file, format!("Field {field} errors: Exclusion rule Entities can't be blank")));
            }
            out.push(Exclusion { scope, filter, entities, dimension_ids: None });
        }
        Ok(out)
    }

    fn inclusion(&mut self, file: &Path, field: &str, fm: &serde_yaml::Mapping) -> Result<Option<Inclusion>> {
        let Some(v) = get(fm, "inclusions") else { return Ok(None) };
        let Some(im) = as_map(v) else {
            return Err(invalid(file, format!("Field {field} errors: inclusions must be a mapping")));
        };
        let filter = parse_filter_mode(str_of(im, "filter")).ok_or_else(|| invalid(file, format!("Field {field} errors: Inclusions Filter should be one of: ignore, apply, only")))?;
        let aggregation = str_of(im, "aggregation").ok_or_else(|| invalid(file, format!("Field {field} errors: Inclusions Aggregation can't be blank")))?;
        let dimensions = strs_of(im, "dimensions");
        if dimensions.is_empty() {
            return Err(invalid(file, format!("Field {field} errors: Inclusions Dimensions can't be blank")));
        }
        Ok(Some(Inclusion { filter, aggregation, dimensions, dimension_ids: None }))
    }

    // ---- models/**/rel.*.yml

    fn relationships(&mut self) -> Result<()> {
        let files = self.files("models", "rel.")?;
        for file in files {
            let v = read_yaml(&file)?;
            let m = as_map(&v).cloned().ok_or_else(|| invalid(&file, "Empty file detected."))?;
            let ds = self.datasource_for(&file, &m, "relationship")?;
            let mut seen_pairs: Vec<(Id, Id)> = Vec::new();
            for (key, def) in &m {
                let Some(key) = key.as_str() else { continue };
                if key == "datasource" {
                    continue;
                }
                let Some(jm) = as_map(def) else {
                    return Err(invalid(&file, format!("JoinDef errors for {key}: must be a mapping")));
                };
                self.unknown_keys(&file, &format!("join {key}"), jm, JOIN_KEYS);
                self.join(&file, ds, key, jm, &mut seen_pairs)?;
            }
        }
        Ok(())
    }

    fn join(&mut self, file: &Path, ds: usize, key: &str, jm: &serde_yaml::Mapping, seen: &mut Vec<(Id, Id)>) -> Result<()> {
        let ds_id = self.datasources[ds].id;
        let find = |name: Option<String>| -> Option<usize> {
            let n = name?.to_lowercase();
            self.tables.iter().position(|t| t.datasource_id == ds_id && t.name.to_lowercase() == n)
        };
        let left_name = str_of(jm, "left").unwrap_or_default();
        let right_name = str_of(jm, "right").unwrap_or_default();
        let left = find(Some(left_name.clone())).ok_or_else(|| invalid(file, format!("Left table '{left_name}' not found in datasource {}", self.datasources[ds].name)))?;
        let right = find(Some(right_name.clone())).ok_or_else(|| invalid(file, format!("Right table '{right_name}' not found in datasource {}", self.datasources[ds].name)))?;
        let (mut left_id, mut right_id) = (self.tables[left].id, self.tables[right].id);
        let pair = (left_id.min(right_id), left_id.max(right_id));
        if seen.contains(&pair) || self.join_defs.iter().any(|j| (j.left_id == left_id && j.right_id == right_id) || (j.left_id == right_id && j.right_id == left_id)) {
            return Err(invalid(file, format!("Join: <{left_name}-{right_name}>, already exists.")));
        }
        seen.push(pair);

        let mut join_type = match str_of(jm, "join").map(|j| j.to_lowercase()).as_deref() {
            None | Some("inner") => JoinType::Inner,
            Some("left") => JoinType::Left,
            Some("right") => JoinType::Right,
            Some(other) => return Err(invalid(file, format!("JoinDef errors for {key}: Join type '{other}' is not included in the list"))),
        };
        let mut cardinality = match str_of(jm, "cardinality").map(|c| c.to_lowercase()).as_deref() {
            Some("one_to_many") => Cardinality::OneToMany,
            Some("many_to_one") => Cardinality::ManyToOne,
            Some("one_to_one") => Cardinality::OneToOne,
            Some(other) => return Err(invalid(file, format!("JoinDef errors for {key}: Cardinality '{other}' is not included in the list"))),
            None => return Err(invalid(file, format!("JoinDef errors for {key}: Cardinality can't be blank"))),
        };
        let mut sql = str_of(jm, "sql").ok_or_else(|| invalid(file, format!("JoinDef errors for {key}: Sql can't be blank")))?.to_lowercase();
        if !sql.contains("left.") || !sql.contains("right.") {
            return Err(invalid(file, format!("JoinDef errors for {key}: Sql join sql should be of the format left.column = right.column")));
        }
        // Paths run from the many side to the one side: a one_to_many join is
        // stored reversed, as `JoinDef#align_join_direction` does.
        if cardinality == Cardinality::OneToMany {
            cardinality = Cardinality::ManyToOne;
            std::mem::swap(&mut left_id, &mut right_id);
            join_type = match join_type {
                JoinType::Left => JoinType::Right,
                JoinType::Right => JoinType::Left,
                JoinType::Inner => JoinType::Inner,
            };
            sql = swap_sides(&sql);
        }
        let cost = self.tables[left].cost + self.tables[right].cost;
        let id = self.join_defs.len() as Id + 1;
        let source = self.source_of(file);
        self.join_defs.push(JoinDef {
            id,
            datasource_id: ds_id,
            left_id,
            right_id,
            sql: sql.clone(),
            cardinality,
            join_type,
            cost,
            allow_measure_expansion: bool_of(jm, "allow_measure_expansion").unwrap_or(false),
            source: source.clone(),
        });
        // A one_to_one join walks both ways (`JoinDef#create_bi_directional_join`).
        if cardinality == Cardinality::OneToOne {
            let id = self.join_defs.len() as Id + 1;
            self.join_defs.push(JoinDef {
                id,
                datasource_id: ds_id,
                left_id: right_id,
                right_id: left_id,
                sql: swap_sides(&sql),
                cardinality,
                join_type: match join_type {
                    JoinType::Left => JoinType::Right,
                    JoinType::Right => JoinType::Left,
                    JoinType::Inner => JoinType::Inner,
                },
                cost,
                allow_measure_expansion: bool_of(jm, "allow_measure_expansion").unwrap_or(false),
                source,
            });
        }
        Ok(())
    }

    // ---- formation

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            project: self.project.clone().unwrap_or(ProjectRef { uid: "project".into(), name: "project".into() }),
            branch: BranchRef { id: 1, uid: self.production_branch.clone(), production: true },
            datasources: self.datasources.clone(),
            tables: self.tables.clone(),
            fields: self.fields.clone(),
            expressions: self.expressions.clone(),
            join_defs: self.join_defs.clone(),
            paths: self.paths.clone(),
            partitions: self.partitions.clone(),
            security_policies: self.policies.clone(),
        }
    }

    fn form(&mut self) -> Result<()> {
        let universe = Universe::build(self.snapshot())?;
        let formed = zsql_formation::form(&universe);
        self.warnings.extend(formed.warnings.iter().map(|w| format!("formation: {w}")));
        self.paths = formed.paths;
        Ok(())
    }

    // ---- what needs paths

    fn post_universe(&mut self) -> Result<()> {
        // Snapshot dates and partitions (`Table#handle_post_universe_setup`).
        let pending_tables = std::mem::take(&mut self.pending_tables);
        for pt in pending_tables {
            if let Some(dim_name) = &pt.snapshot {
                let dim = self.dimension_by_name(dim_name).ok_or_else(|| invalid(&pt.file, format!("could not find snapshot dimension {dim_name} in branch.")))?;
                self.tables[pt.table_index].snapshot_date_id = Some(self.fields[dim].id);
            }
            for pm in &pt.partitions {
                let dim_name = str_of(pm, "dimension").ok_or_else(|| invalid(&pt.file, "Partition errors: Dimension must exist"))?;
                let dim = self.dimension_by_name(&dim_name).ok_or_else(|| invalid(&pt.file, format!("Partition errors: Dimension must exist ({dim_name})")))?;
                let predicate: Predicate = str_of(pm, "predicate")
                    .and_then(|p| serde_json::from_value(Json::String(p.to_lowercase())).ok())
                    .ok_or_else(|| invalid(&pt.file, format!("Partition errors: Predicate is not included in the list ({dim_name})")))?;
                // Ranges need a number or date dimension to compare on; a
                // list partition works on any type.
                let ranged = matches!(predicate, Predicate::Between | Predicate::GreaterThan | Predicate::GreaterThanOrEqualTo | Predicate::LessThan | Predicate::LessThanOrEqualTo);
                if !ranged && predicate != Predicate::InList {
                    self.warn(&pt.file, format!("partition on {dim_name}: predicate {} is not supported (between, greater_than, greater_than_or_equal_to, less_than, less_than_or_equal_to, in_list); the partition will not rank the table", predicate.name()));
                }
                let numeric_or_date = matches!(self.fields[dim].data_type, Some(DataType::Integer | DataType::Decimal | DataType::Bigint | DataType::Date | DataType::DateTime));
                if ranged && !numeric_or_date {
                    self.warn(&pt.file, format!("partition on {dim_name}: a {} partition needs a number or date dimension", predicate.name()));
                }
                let id = self.partitions.len() as Id + 1;
                self.partitions.push(Partition {
                    id,
                    table_id: self.tables[pt.table_index].id,
                    dimension_id: self.fields[dim].id,
                    predicate,
                    filter_value: str_of(pm, "filter_value"),
                    filter_value_end: str_of(pm, "filter_value_end"),
                });
            }
        }

        // Expression references (`Expression::References#compute_and_validate_references`)
        // and their integrity across universes.
        let pending = std::mem::take(&mut self.pending_expressions);
        // Which file each referencing expression came from, kept for the
        // cycle check below, which runs once every reference is resolved.
        let mut ref_files: HashMap<Id, PathBuf> = HashMap::new();
        for pe in pending {
            let mut measure_ids = Vec::new();
            for m in &pe.measure_refs {
                let i = self.field_by_name(FieldKind::Measure, m).ok_or_else(|| invalid(&pe.file, format!("[{}]: could not find measure named: {m}", pe.field_name)))?;
                measure_ids.push(self.fields[i].id);
            }
            let mut dimension_ids = Vec::new();
            for d in &pe.dimension_refs {
                let i = self.field_by_name(FieldKind::Dimension, d).ok_or_else(|| invalid(&pe.file, format!("[{}]: could not find dimension named: {d}", pe.field_name)))?;
                dimension_ids.push(self.fields[i].id);
            }
            // `[Name]` with no suffix. A name belongs to at most one
            // dimension and at most one measure, so it is only ambiguous
            // when both exist; that is the one case the suffix is for.
            // Before this, a bare reference was parsed and then dropped,
            // so it reached the warehouse as literal `[Name]` text.
            for b in &pe.bare_refs {
                let dim = self.field_by_name(FieldKind::Dimension, b);
                let msr = self.field_by_name(FieldKind::Measure, b);
                match (dim, msr) {
                    (Some(d), Some(_)) => {
                        // The parser lower-cases the formula, so the name
                        // is echoed back from the field, not from `b`.
                        let n = &self.fields[d].name;
                        return Err(invalid(
                            &pe.file,
                            format!("[{}]: [{n}] is both a dimension and a measure. Write [{n}]@d or [{n}]@m to say which.", pe.field_name),
                        ));
                    }
                    (Some(i), None) => dimension_ids.push(self.fields[i].id),
                    (None, Some(i)) => measure_ids.push(self.fields[i].id),
                    (None, None) => {
                        return Err(invalid(
                            &pe.file,
                            format!(
                                "[{}]: could not find a field named: {b}. Square brackets name a field; for a warehouse column write it bare or in double quotes.",
                                pe.field_name
                            ),
                        ))
                    }
                }
            }
            let e = &self.expressions[pe.expression_index];
            let own = e.field_id;
            let has_columns = !e.column_refs.is_empty();
            let table_name = self.tables.iter().find(|t| t.id == e.table_id).map(|t| t.name.clone()).unwrap_or_default();
            if !dimension_ids.is_empty() && measure_ids.is_empty() {
                let mut fields = vec![own];
                fields.extend(dimension_ids.iter().copied());
                if !self.some_universe_has_all(&fields) {
                    return Err(invalid(&pe.file, format!("[{}] on {table_name}: Sql Referenced dimensions in the formula could not be located in a single Universe.", pe.field_name)));
                }
            }
            if !measure_ids.is_empty() && dimension_ids.is_empty() && !has_columns {
                let mut fields = vec![own];
                fields.extend(measure_ids.iter().copied());
                if !self.some_datasource_has_all_measures(&fields) {
                    return Err(invalid(&pe.file, format!("[{}] on {table_name}: Sql Could not find a datasource which had all of the required measures. Cross datasource queries are not supported.", pe.field_name)));
                }
            }
            if !measure_ids.is_empty() && (!dimension_ids.is_empty() || has_columns) {
                let mut fields = vec![own];
                fields.extend(measure_ids.iter().copied());
                fields.extend(dimension_ids.iter().copied());
                if !self.some_universe_has_all(&fields) {
                    return Err(invalid(&pe.file, format!("[{}] on {table_name}: Sql Mixed measure and dimension references must all be reachable from one universe.", pe.field_name)));
                }
            }
            ref_files.entry(own).or_insert_with(|| pe.file.clone());
            let e = &mut self.expressions[pe.expression_index];
            e.measure_ref_ids = measure_ids;
            e.dimension_ref_ids = dimension_ids;
        }
        self.reject_reference_cycles(&ref_files)?;

        // Exclusion and inclusion rules resolve their entities to dimensions.
        let rules = std::mem::take(&mut self.pending_rules);
        for pr in rules {
            let field_name = self.fields[pr.field_index].name.clone();
            let measure_id = self.fields[pr.field_index].id;
            let exclusions = self.fields[pr.field_index].exclusions.clone();
            let mut resolved = Vec::new();
            for mut ex in exclusions {
                let names: Vec<String> = ex.entities.iter().map(|e| e.trim().to_lowercase()).collect();
                let (found, dims): (Vec<String>, Vec<Id>) = match ex.scope {
                    ExclusionScope::Dimension => {
                        let hits: Vec<usize> = names.iter().filter_map(|n| self.field_by_name(FieldKind::Dimension, n)).collect();
                        (hits.iter().map(|&i| self.fields[i].name.to_lowercase()).collect(), hits.iter().map(|&i| self.fields[i].id).collect())
                    }
                    ExclusionScope::Table => {
                        let tables: Vec<Id> = self.tables.iter().filter(|t| names.contains(&t.name.to_lowercase())).map(|t| t.id).collect();
                        let mut dims: Vec<Id> = self
                            .expressions
                            .iter()
                            .filter(|e| tables.contains(&e.table_id))
                            .filter(|e| self.field_kind(e.field_id) == Some(FieldKind::Dimension))
                            .map(|e| e.field_id)
                            .collect();
                        dims.sort();
                        dims.dedup();
                        (self.tables.iter().filter(|t| tables.contains(&t.id)).map(|t| t.name.to_lowercase()).collect(), dims)
                    }
                    ExclusionScope::Universe => {
                        let tables: Vec<Id> = self.tables.iter().filter(|t| names.contains(&t.name.to_lowercase())).map(|t| t.id).collect();
                        let mut dims: Vec<Id> = self.paths.iter().filter(|p| tables.contains(&p.root_table_id) && p.field_type == FieldKind::Dimension).map(|p| p.field_id).collect();
                        dims.sort();
                        dims.dedup();
                        (self.tables.iter().filter(|t| tables.contains(&t.id)).map(|t| t.name.to_lowercase()).collect(), dims)
                    }
                };
                let missing: Vec<&String> = names.iter().filter(|n| !found.contains(n)).collect();
                if !missing.is_empty() {
                    let what = match ex.scope {
                        ExclusionScope::Dimension => "dimensions",
                        ExclusionScope::Table => "tables",
                        ExclusionScope::Universe => "universes",
                    };
                    return Err(invalid(&pr.file, format!("[{field_name}]: Exclusion rule Entities Could not find these {what}: {}", missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))));
                }
                ex.dimension_ids = Some(dims);
                resolved.push(ex);
            }
            self.fields[pr.field_index].exclusions = resolved;

            if let Some(mut inc) = self.fields[pr.field_index].inclusions.clone() {
                let mut ids = Vec::new();
                let mut missing = Vec::new();
                for d in &inc.dimensions {
                    match self.field_by_name(FieldKind::Dimension, d) {
                        Some(i) => ids.push(self.fields[i].id),
                        None => missing.push(d.clone()),
                    }
                }
                if !missing.is_empty() {
                    return Err(invalid(&pr.file, format!("[{field_name}]: Inclusions Dimensions {} could not be found", missing.join(", "))));
                }
                let mut all = vec![measure_id];
                all.extend(ids.iter().copied());
                if !self.some_universe_has_all(&all) {
                    return Err(invalid(&pr.file, format!("[{field_name}]: Inclusions Dimensions {:?} could not be found in a universe with this measure.", inc.dimensions)));
                }
                inc.dimension_ids = Some(ids);
                self.fields[pr.field_index].inclusions = Some(inc);
            }
        }
        Ok(())
    }

    fn field_kind(&self, id: Id) -> Option<FieldKind> {
        self.fields.iter().find(|f| f.id == id).map(|f| f.kind)
    }

    /// A field whose expression reaches itself through other fields.
    ///
    /// The planner builds a reference per referenced field and recurses,
    /// so a cycle does not fail: it overflows the stack and takes the
    /// process down with it, from one query. Caught here, where the
    /// modeller can still read the names.
    fn reject_reference_cycles(&self, files: &HashMap<Id, PathBuf>) -> Result<()> {
        // Field -> the fields its expressions name. A field may be
        // expressed on several tables, and any one of them can close a
        // cycle, so every expression counts.
        let mut edges: HashMap<Id, Vec<Id>> = HashMap::new();
        for e in &self.expressions {
            let to = edges.entry(e.field_id).or_default();
            for r in e.dimension_ref_ids.iter().chain(e.measure_ref_ids.iter()) {
                if !to.contains(r) {
                    to.push(*r);
                }
            }
        }
        let name_of = |id: Id| self.fields.iter().find(|f| f.id == id).map(|f| f.name.clone()).unwrap_or_else(|| id.to_string());

        // Iterative depth-first search, since recursing to find a stack
        // overflow would be its own joke.
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Open,
            Done,
        }
        let mut mark: HashMap<Id, Mark> = HashMap::new();
        for start in edges.keys().copied() {
            if mark.contains_key(&start) {
                continue;
            }
            let mut path: Vec<Id> = Vec::new();
            let mut stack: Vec<(Id, usize)> = vec![(start, 0)];
            mark.insert(start, Mark::Open);
            path.push(start);
            while let Some((node, next)) = stack.pop() {
                let kids = edges.get(&node).map(Vec::as_slice).unwrap_or(&[]);
                if next >= kids.len() {
                    mark.insert(node, Mark::Done);
                    path.pop();
                    continue;
                }
                stack.push((node, next + 1));
                let kid = kids[next];
                match mark.get(&kid) {
                    Some(Mark::Done) => {}
                    Some(Mark::Open) => {
                        // `path` from the first sighting of `kid` is the cycle.
                        let at = path.iter().position(|p| *p == kid).unwrap_or(0);
                        let mut names: Vec<String> = path[at..].iter().map(|i| name_of(*i)).collect();
                        names.push(name_of(kid));
                        let file = files.get(&node).cloned().unwrap_or_default();
                        return Err(invalid(
                            &file,
                            if names.len() == 2 {
                                format!("[{}]: its expression references itself.", names[0])
                            } else {
                                format!("[{}]: its expression references itself through {}.", names[0], names[1..].join(" -> "))
                            },
                        ));
                    }
                    None => {
                        mark.insert(kid, Mark::Open);
                        path.push(kid);
                        stack.push((kid, 0));
                    }
                }
            }
        }
        Ok(())
    }

    fn field_by_name(&self, kind: FieldKind, name: &str) -> Option<usize> {
        self.field_index.get(&(kind, name.trim().to_lowercase())).copied()
    }

    fn dimension_by_name(&self, name: &str) -> Option<usize> {
        self.field_by_name(FieldKind::Dimension, name)
    }

    /// Some root table reaches every one of the fields.
    fn some_universe_has_all(&self, fields: &[Id]) -> bool {
        let mut by_root: HashMap<Id, HashSet<Id>> = HashMap::new();
        for p in &self.paths {
            if fields.contains(&p.field_id) {
                by_root.entry(p.root_table_id).or_default().insert(p.field_id);
            }
        }
        by_root.values().any(|s| fields.iter().all(|f| s.contains(f)))
    }

    /// Some datasource holds a measure path for every one of the fields.
    fn some_datasource_has_all_measures(&self, fields: &[Id]) -> bool {
        let mut by_ds: HashMap<Id, HashSet<Id>> = HashMap::new();
        for p in &self.paths {
            if p.field_type == FieldKind::Measure && fields.contains(&p.field_id) {
                by_ds.entry(p.datasource_id).or_default().insert(p.field_id);
            }
        }
        by_ds.values().any(|s| fields.iter().all(|f| s.contains(f)))
    }

    // ---- security.yml

    fn security(&mut self) -> Result<()> {
        let file = self.dir.join("security.yml");
        if !file.exists() {
            return Ok(());
        }
        let v = read_yaml(&file)?;
        let m = as_map(&v).ok_or_else(|| invalid(&file, "security.yml must have a 'policies' key."))?;
        let policies = match get(m, "policies") {
            Some(Yaml::Sequence(items)) => items.clone(),
            Some(Yaml::Null) | None => return Err(invalid(&file, "security.yml must have a 'policies' key.")),
            Some(_) => return Err(invalid(&file, "policies must be a list")),
        };
        for p in &policies {
            let Some(pm) = as_map(p) else {
                return Err(invalid(&file, "Each policy must be a mapping."));
            };
            let name = str_of(pm, "name").ok_or_else(|| invalid(&file, "Each policy must have a name."))?;
            let mode = match str_of(pm, "mode").map(|s| s.to_lowercase()).as_deref() {
                Some("mask") | Some("mask_data") => PolicyMode::MaskData,
                Some("filter") | Some("filter_data") => PolicyMode::FilterData,
                other => return Err(invalid(&file, format!("Unknown mode '{}'. Use mask_data or filter_data.", other.unwrap_or("")))),
            };
            let context = str_of(pm, "context_dimension").ok_or_else(|| invalid(&file, format!("Policy '{name}' is missing context_dimension.")))?;
            let context_uid = self
                .dimension_by_name(&context)
                .map(|i| self.fields[i].uid.clone())
                .ok_or_else(|| invalid(&file, format!("Policy '{name}': context_dimension '{context}' not found in this branch.")))?;
            let triggers = get(pm, "triggers").and_then(as_map).cloned().unwrap_or_default();
            let trigger_tags = strs_of(&triggers, "field_tags");
            let mut trigger_field_uids = Vec::new();
            for fname in strs_of(&triggers, "field_names") {
                let i = self
                    .field_by_name(FieldKind::Dimension, &fname)
                    .or_else(|| self.field_by_name(FieldKind::Measure, &fname))
                    .ok_or_else(|| invalid(&file, format!("Policy '{name}': trigger field '{fname}' not found in this branch.")))?;
                trigger_field_uids.push(self.fields[i].uid.clone());
            }
            let resolution = get(pm, "permission_resolution").and_then(as_map).cloned().unwrap_or_default();
            let permission_source = match str_of(&resolution, "source").map(|s| s.to_lowercase()).as_deref() {
                None | Some("groups") => PermissionSource::Groups,
                Some("user") => PermissionSource::User,
                Some(other) => return Err(invalid(&file, format!("Policy '{name}' errors: Permission source '{other}' is not included in the list"))),
            };
            let permission_value_from = match str_of(&resolution, "value_from").map(|s| s.to_lowercase()).as_deref() {
                Some("name") | Some("from_name") => PermissionValueFrom::FromName,
                Some("tag") | Some("from_tag") => PermissionValueFrom::FromTag,
                Some("email") | Some("from_email") => PermissionValueFrom::FromEmail,
                other => return Err(invalid(&file, format!("Policy '{name}' errors: Permission value from '{}' is not included in the list", other.unwrap_or("")))),
            };
            let unresolved = match str_of(&resolution, "unresolved").map(|s| s.to_lowercase()).as_deref() {
                None | Some("deny") => Unresolved::Deny,
                Some("allow") => Unresolved::Allow,
                Some(other) => return Err(invalid(&file, format!("Policy '{name}': unresolved '{other}' should be deny or allow"))),
            };
            let bypass = get(pm, "bypass").and_then(as_map).cloned().unwrap_or_default();
            let uid = self.policy_slugs.take(&[name.clone()]);
            self.policies.push(SecurityPolicy {
                uid,
                name,
                mode,
                mask_value: Some(str_of(pm, "mask_value").unwrap_or_else(|| "#######".into())),
                context_dimension_uid: context_uid,
                trigger_tags,
                trigger_field_uids,
                permission_source,
                permission_value_from,
                permission_tag_key: str_of(&resolution, "tag_key"),
                bypass_system_admin: bool_of(&bypass, "system_admin").unwrap_or(true),
                bypass_project_admin: bool_of(&bypass, "project_admin").unwrap_or(false),
                unresolved,
            });
        }
        Ok(())
    }

    // ---- tests/*.yml

    fn tests(&mut self) -> Result<()> {
        let dir = self.dir.join("tests");
        if !dir.is_dir() {
            return Ok(());
        }
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|e| ProjectError::Io(dir.display().to_string(), e))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("yml") | Some("yaml")))
            .collect();
        files.sort();
        for file in files {
            let v = read_yaml(&file)?;
            let Some(m) = as_map(&v) else { continue };
            let name = str_of(m, "name").ok_or_else(|| invalid(&file, "Test must have a name"))?;
            let projections = strs_of(m, "projections");
            if projections.is_empty() {
                return Err(invalid(&file, "Test must have at least one field in 'projections'"));
            }
            let assert_sql = str_of(m, "assert_sql");
            let assert_regex = str_of(m, "assert_regex");
            if assert_sql.is_none() && assert_regex.is_none() {
                return Err(invalid(&file, "Test must have at least one assertion (assert_sql or assert_regex)"));
            }
            self.tests.push(ProjectTest { file, name, projections, assert_sql, assert_regex });
        }
        Ok(())
    }

    fn finish(self) -> Loaded {
        let snapshot = self.snapshot();
        let values_path = [self.dir.join("samples").join(values::FILE_NAME), self.dir.join(values::FILE_NAME)].into_iter().find(|p| p.exists()).unwrap_or_else(|| self.dir.join("samples").join(values::FILE_NAME));
        let values = match values::ValueCatalog::read(&values_path) {
            Ok(v) => v,
            Err(e) => {
                let mut warnings = self.warnings;
                warnings.push(format!("{}: {e}", values::FILE_NAME));
                return Loaded { snapshot, warnings, tests: self.tests, values: None };
            }
        };
        Loaded { snapshot, warnings: self.warnings, tests: self.tests, values }
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            walk(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// `left.` and `right.` exchanged, for a join stored in the other direction.
fn swap_sides(sql: &str) -> String {
    sql.replace("left.", "\u{0}xx.").replace("right.", "left.").replace("\u{0}xx.", "right.")
}

/// Rails validates a measure's SQL against `/\A.*\w+\(.*\).*\z/`.
fn measure_has_function(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'(' && i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
            return sql[i..].contains(')');
        }
        i += 1;
    }
    false
}

fn parse_filter_mode(s: Option<String>) -> Option<FilterMode> {
    match s.map(|s| s.to_lowercase()).as_deref() {
        Some("ignore") => Some(FilterMode::Ignore),
        Some("apply") => Some(FilterMode::Apply),
        Some("only") => Some(FilterMode::Only),
        _ => None,
    }
}

/// Rails `FormatParseable#parse_format_syntax`: `number:2:abbreviate`,
/// `currency:2:USD`, `percent:1`, `date:short`, `datetime:iso`, `html:...`,
/// `javascript:...`.
pub fn parse_format_syntax(syntax: &str) -> Option<Json> {
    let syntax = syntax.trim();
    if syntax.is_empty() {
        return None;
    }
    let parts: Vec<&str> = syntax.split(':').collect();
    let mut f = serde_json::Map::new();
    let precision = |p: Option<&&str>| p.filter(|s| !s.is_empty() && **s != "abbreviate").and_then(|s| s.parse::<i64>().ok());
    match parts[0] {
        "number" => {
            f.insert("type".into(), Json::from("number"));
            if let Some(p) = precision(parts.get(1)) {
                f.insert("precision".into(), Json::from(p));
            }
            if parts.contains(&"abbreviate") {
                f.insert("abbreviate".into(), Json::from(true));
            }
        }
        "currency" => {
            f.insert("type".into(), Json::from("currency"));
            f.insert("precision".into(), Json::from(precision(parts.get(1)).unwrap_or(2)));
            if let Some(unit) = parts.get(2).filter(|s| !s.is_empty() && **s != "abbreviate") {
                f.insert("unit".into(), Json::from(*unit));
            }
            if parts.contains(&"abbreviate") {
                f.insert("abbreviate".into(), Json::from(true));
            }
        }
        "percent" => {
            f.insert("type".into(), Json::from("percent"));
            f.insert("precision".into(), Json::from(precision(parts.get(1)).unwrap_or(2)));
        }
        "date" => {
            f.insert("type".into(), Json::from("date"));
            let pattern = match parts.get(1).copied() {
                Some("short") => "%b %d, %Y".to_string(),
                Some("long") => "%B %d, %Y".to_string(),
                Some("iso") | None | Some("") => "%Y-%m-%d".to_string(),
                Some(style) => style.to_string(),
            };
            f.insert("pattern".into(), Json::from(pattern));
        }
        "datetime" => {
            f.insert("type".into(), Json::from("datetime"));
            let pattern = match parts.get(1).copied() {
                Some("short") => "%b %d, %Y %I:%M %p".to_string(),
                Some("long") => "%B %d, %Y at %I:%M:%S %p".to_string(),
                Some("iso") | None | Some("") => "%Y-%m-%d %H:%M:%S".to_string(),
                Some(style) => style.to_string(),
            };
            f.insert("pattern".into(), Json::from(pattern));
        }
        "html" => {
            f.insert("type".into(), Json::from("html"));
            if parts.len() > 1 && !parts[1].is_empty() {
                f.insert("template".into(), Json::from(parts[1..].join(":")));
            }
        }
        "javascript" => {
            f.insert("type".into(), Json::from("javascript"));
            if parts.len() > 1 && !parts[1].is_empty() {
                f.insert("function".into(), Json::from(parts[1..].join(":")));
            }
        }
        _ => return None,
    }
    Some(Json::Object(f))
}

/// Rails `normalize_format_hash`: blanks dropped, precision an integer,
/// abbreviate kept only when true.
fn normalize_format(v: Json) -> Option<Json> {
    let Json::Object(m) = v else { return None };
    let mut out = serde_json::Map::new();
    for (k, v) in m {
        match (k.as_str(), &v) {
            (_, Json::Null) => {}
            (_, Json::String(s)) if s.is_empty() => {}
            ("precision", Json::Number(n)) => {
                out.insert(k, Json::from(n.as_f64().unwrap_or(0.0) as i64));
            }
            ("precision", Json::String(s)) => {
                out.insert(k, Json::from(s.trim().parse::<i64>().unwrap_or(0)));
            }
            ("abbreviate", Json::Bool(true)) => {
                out.insert(k, Json::from(true));
            }
            ("abbreviate", Json::String(s)) if matches!(s.to_lowercase().as_str(), "true" | "1" | "yes") => {
                out.insert(k, Json::from(true));
            }
            ("abbreviate", _) => {}
            _ => {
                out.insert(k, v);
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(Json::Object(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameterize_matches_active_support() {
        assert_eq!(parameterize("Store Net Paid"), "store-net-paid");
        assert_eq!(parameterize("ss_net_paid"), "ss_net_paid");
        assert_eq!(parameterize("  % Margin (net) "), "margin-net");
        assert_eq!(parameterize("Enrollment (K-12)"), "enrollment-k-12");
    }

    #[test]
    fn slugs_fall_back_to_the_next_candidate() {
        let mut s = Slugs::new();
        assert_eq!(s.take(&["Date".into(), "dimension-Date".into()]), "date");
        assert_eq!(s.take(&["Date".into(), "measure-Date".into()]), "measure-date");
        assert_eq!(s.take(&["Date".into(), "measure-Date".into()]), "date-2");
    }

    #[test]
    fn join_sides_swap() {
        assert_eq!(swap_sides("left.id = right.customer_id"), "right.id = left.customer_id");
    }

    #[test]
    fn format_shortcuts() {
        assert_eq!(parse_format_syntax("currency:2"), Some(serde_json::json!({"type": "currency", "precision": 2})));
        assert_eq!(parse_format_syntax("number:2:abbreviate"), Some(serde_json::json!({"type": "number", "precision": 2, "abbreviate": true})));
        assert_eq!(parse_format_syntax("date:short"), Some(serde_json::json!({"type": "date", "pattern": "%b %d, %Y"})));
        assert_eq!(parse_format_syntax("nope"), None);
    }

    #[test]
    fn measure_function_rule() {
        assert!(measure_has_function("sum(ss_net_paid)"));
        assert!(measure_has_function("count(distinct x) * 1.0"));
        assert!(!measure_has_function("ss_net_paid"));
    }
}

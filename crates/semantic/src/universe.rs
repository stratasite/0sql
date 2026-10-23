use crate::snapshot::*;
use std::collections::HashMap;

/// A branch's semantic model, indexed for lookup and checked for
/// referential integrity. Immutable once built; share it behind an `Arc`.
#[derive(Debug)]
pub struct Universe {
    snap: Snapshot,
    tables_by_id: HashMap<Id, usize>,
    tables_by_uid: HashMap<String, usize>,
    fields_by_id: HashMap<Id, usize>,
    fields_by_uid: HashMap<(FieldKind, String), usize>,
    fields_by_name: HashMap<(FieldKind, String), usize>,
    datasources_by_id: HashMap<Id, usize>,
    joins_by_id: HashMap<Id, usize>,
    expressions_by_table: HashMap<Id, Vec<usize>>,
    expression_by_table_field: HashMap<(Id, Id), usize>,
    warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("snapshot has {} integrity problems:\n{}", .0.len(), .0.join("\n"))]
    Integrity(Vec<String>),
}

impl Universe {
    pub fn build(snap: Snapshot) -> Result<Self, BuildError> {
        let mut problems = Vec::new();
        let mut warnings = Vec::new();

        // Every id is unique, or the index would keep whichever came
        // last and silently answer for both.
        fn by_id<T>(items: &[T], id: impl Fn(&T) -> Id, what: &str, problems: &mut Vec<String>) -> HashMap<Id, usize> {
            let mut out = HashMap::new();
            for (i, item) in items.iter().enumerate() {
                if out.insert(id(item), i).is_some() {
                    problems.push(format!("duplicate {what} id {}", id(item)));
                }
            }
            out
        }
        let datasources_by_id = by_id(&snap.datasources, |d| d.id, "datasource", &mut problems);
        let tables_by_id = by_id(&snap.tables, |t| t.id, "table", &mut problems);
        let fields_by_id = by_id(&snap.fields, |f| f.id, "field", &mut problems);
        let joins_by_id = by_id(&snap.join_defs, |j| j.id, "join", &mut problems);
        // Uids are looked up trimmed and lower-cased, so they are kept
        // that way.
        let mut tables_by_uid = HashMap::new();
        for (i, t) in snap.tables.iter().enumerate() {
            if tables_by_uid.insert(t.uid.trim().to_lowercase(), i).is_some() {
                problems.push(format!("duplicate table uid {}", t.uid));
            }
            if t.cost < 0 {
                problems.push(format!("table {} has a negative cost ({}); costs rank routes and must not be negative", t.uid, t.cost));
            }
        }
        let mut fields_by_uid = HashMap::new();
        let mut fields_by_name = HashMap::new();
        for (i, f) in snap.fields.iter().enumerate() {
            if fields_by_uid.insert((f.kind, f.uid.trim().to_lowercase()), i).is_some() {
                problems.push(format!("duplicate {:?} uid {}", f.kind, f.uid));
            }
            if fields_by_name.insert((f.kind, f.name.trim().to_lowercase()), i).is_some() {
                problems.push(format!("duplicate {:?} name {}", f.kind, f.name));
            }
        }
        for j in &snap.join_defs {
            if j.cost < 0 {
                problems.push(format!("join {} has a negative cost ({})", j.id, j.cost));
            }
        }

        for t in &snap.tables {
            if !datasources_by_id.contains_key(&t.datasource_id) {
                problems.push(format!("table {} references missing datasource {}", t.uid, t.datasource_id));
            }
            if let Some(sd) = t.snapshot_date_id {
                match fields_by_id.get(&sd).map(|&i| &snap.fields[i]) {
                    Some(f) if f.is_dimension() => {}
                    Some(f) => problems.push(format!("table {} snapshot date {} is not a dimension", t.uid, f.uid)),
                    None => problems.push(format!("table {} references missing snapshot date {}", t.uid, sd)),
                }
            }
        }

        let mut expressions_by_table: HashMap<Id, Vec<usize>> = HashMap::new();
        let mut expression_by_table_field = HashMap::new();
        for (i, e) in snap.expressions.iter().enumerate() {
            if !tables_by_id.contains_key(&e.table_id) {
                problems.push(format!("expression {} references missing table {}", e.id, e.table_id));
            }
            if !fields_by_id.contains_key(&e.field_id) {
                problems.push(format!("expression {} references missing field {}", e.id, e.field_id));
            }
            expressions_by_table.entry(e.table_id).or_default().push(i);
            if expression_by_table_field.insert((e.table_id, e.field_id), i).is_some() {
                problems.push(format!("field {} mapped twice on table {}", e.field_id, e.table_id));
            }
        }

        for j in &snap.join_defs {
            for (side, id) in [("left", j.left_id), ("right", j.right_id)] {
                if !tables_by_id.contains_key(&id) {
                    problems.push(format!("join {} {} table {} is missing", j.id, side, id));
                }
            }
            if !datasources_by_id.contains_key(&j.datasource_id) {
                problems.push(format!("join {} references missing datasource {}", j.id, j.datasource_id));
            }
        }

        for p in &snap.partitions {
            if !tables_by_id.contains_key(&p.table_id) {
                problems.push(format!("partition {} references missing table {}", p.id, p.table_id));
            }
            match fields_by_id.get(&p.dimension_id).map(|&i| &snap.fields[i]) {
                Some(f) if f.is_dimension() => {}
                _ => problems.push(format!("partition {} dimension {} is missing or not a dimension", p.id, p.dimension_id)),
            }
        }

        for path in &snap.paths {
            for id in [path.root_table_id, path.target_table_id] {
                if !tables_by_id.contains_key(&id) {
                    problems.push(format!("path {} references missing table {}", path.id, id));
                }
            }
            if !fields_by_id.contains_key(&path.field_id) {
                problems.push(format!("path {} references missing field {}", path.id, path.field_id));
            }
            for j in &path.joins {
                if !joins_by_id.contains_key(j) {
                    problems.push(format!("path {} references missing join {}", path.id, j));
                }
            }
        }

        // Rails skips a policy whose context dimension does not resolve; so do we.
        for pol in &snap.security_policies {
            if !fields_by_uid.contains_key(&(FieldKind::Dimension, pol.context_dimension_uid.trim().to_lowercase())) {
                warnings.push(format!("policy {} context dimension {} not found; policy is inert", pol.uid, pol.context_dimension_uid));
            }
        }

        if !problems.is_empty() {
            return Err(BuildError::Integrity(problems));
        }

        Ok(Self {
            snap,
            tables_by_id,
            tables_by_uid,
            fields_by_id,
            fields_by_uid,
            fields_by_name,
            datasources_by_id,
            joins_by_id,
            expressions_by_table,
            expression_by_table_field,
            warnings,
        })
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snap
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn datasource(&self, id: Id) -> Option<&Datasource> {
        self.datasources_by_id.get(&id).map(|&i| &self.snap.datasources[i])
    }

    pub fn table(&self, id: Id) -> Option<&Table> {
        self.tables_by_id.get(&id).map(|&i| &self.snap.tables[i])
    }

    pub fn table_by_uid(&self, uid: &str) -> Option<&Table> {
        self.tables_by_uid.get(&uid.trim().to_lowercase()).map(|&i| &self.snap.tables[i])
    }

    /// A table by its uid or its name, ignoring case and the spacing and
    /// punctuation that separate words, so a hint can be written either way.
    pub fn table_by_uid_or_name(&self, term: &str) -> Option<&Table> {
        if let Some(t) = self.table_by_uid(term) {
            return Some(t);
        }
        let want = squash(term);
        if want.is_empty() {
            return None;
        }
        self.snap.tables.iter().find(|t| squash(&t.name) == want || squash(&t.uid) == want)
    }

    pub fn field(&self, id: Id) -> Option<&Field> {
        self.fields_by_id.get(&id).map(|&i| &self.snap.fields[i])
    }

    /// A field by uid. With `kind` unset, a dimension wins over a measure of
    /// the same uid; callers that must disambiguate pass the kind.
    pub fn field_by_uid(&self, uid: &str, kind: Option<FieldKind>) -> Option<&Field> {
        let key = uid.trim().to_lowercase();
        let lookup = |k: FieldKind| self.fields_by_uid.get(&(k, key.clone())).map(|&i| &self.snap.fields[i]);
        match kind {
            Some(k) => lookup(k),
            None => lookup(FieldKind::Dimension).or_else(|| lookup(FieldKind::Measure)),
        }
    }

    /// A field by display name, case-insensitive (Rails `named`).
    pub fn field_by_name(&self, name: &str, kind: FieldKind) -> Option<&Field> {
        self.fields_by_name.get(&(kind, name.trim().to_lowercase())).map(|&i| &self.snap.fields[i])
    }

    pub fn join(&self, id: Id) -> Option<&JoinDef> {
        self.joins_by_id.get(&id).map(|&i| &self.snap.join_defs[i])
    }

    pub fn expressions_of(&self, table_id: Id) -> impl Iterator<Item = &Expression> {
        self.expressions_by_table.get(&table_id).into_iter().flatten().map(move |&i| &self.snap.expressions[i])
    }

    pub fn expression_for(&self, table_id: Id, field_id: Id) -> Option<&Expression> {
        self.expression_by_table_field.get(&(table_id, field_id)).map(|&i| &self.snap.expressions[i])
    }

    /// A path's stable identity: the table uids along its route from the
    /// root, then the field uid (and the blending field for a blend path).
    /// Names, never row ids, so equal-cost ties break the same way on every
    /// branch and every deploy.
    pub fn path_signature(&self, p: &Path) -> String {
        let uid = |id: Id| self.table(id).map(|t| t.uid.as_str()).unwrap_or("?").to_string();
        let mut route = vec![uid(p.root_table_id)];
        for j in &p.joins {
            if let Some(jd) = self.join(*j) {
                route.push(uid(jd.right_id));
            }
        }
        let field = self.field(p.field_id).map(|f| f.uid.as_str()).unwrap_or("?");
        let mut sig = format!("{}:{}", route.join(">"), field);
        if let Some(b) = p.blending_field_id {
            sig.push('~');
            sig.push_str(self.field(b).map(|f| f.uid.as_str()).unwrap_or("?"));
        }
        sig
    }

    pub fn tables(&self) -> &[Table] {
        &self.snap.tables
    }

    pub fn fields(&self) -> &[Field] {
        &self.snap.fields
    }

    pub fn joins(&self) -> &[JoinDef] {
        &self.snap.join_defs
    }

    pub fn policies(&self) -> &[SecurityPolicy] {
        &self.snap.security_policies
    }
}

/// A name reduced to its letters and digits, lowercased: `Fact Transactions`,
/// `fact-transactions` and `fact_transactions` all squash to the same thing.
fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

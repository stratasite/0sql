//! The query bound to a semantic model: every projection and filter resolved to
//! its semantic field. Components are shared by `Rc` so a node's wrapper and
//! the query's own list refer to the same record, as Rails' object identity does.

use crate::{Error, Result};
use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;
use zsql_semantic::{Field, FieldKind, Id, SemanticModel, Table};
use zsql_spec::{Decorator, DecoratorKind, Filter, LinkMode, LinkTarget, Projection, Query, SecurityContext, Tree};

thread_local! {
    static NEXT_PID: Cell<usize> = const { Cell::new(0) };
}

fn next_pid() -> usize {
    NEXT_PID.with(|c| {
        let v = c.get();
        c.set(v + 1);
        v
    })
}

#[derive(Debug)]
pub struct ResolvedProjection<'a> {
    /// Identity: two wrappers of the same projection share a pid.
    pub pid: usize,
    pub def: Projection,
    pub field: Option<&'a Field>,
    /// Fields a calculation references, resolved.
    pub field_refs: Vec<&'a Field>,
    /// Engine-created projections carry no ordinal and never render for the user.
    pub is_static: bool,
    /// The persisted projection an engine calculation descends from.
    pub source: RefCell<Option<Rc<ResolvedProjection<'a>>>>,
    /// Whether the projection is a saved record (engine-built ones are not).
    pub persisted: bool,
    /// Measure-level segment links on this projection: (segment sid, mode).
    pub links: Vec<(usize, LinkMode)>,
    /// For a carried expansion column: the definition projection's pid and its segment.
    pub carried_from: Option<(usize, usize)>,
}

impl<'a> ResolvedProjection<'a> {
    pub fn for_field(field: &'a Field) -> Self {
        let def = Projection {
            reference: String::new(),
            kind: zsql_spec::ProjectionKind::Field,
            field_uid: Some(field.uid.clone()),
            field_kind: Some(field.kind),
            alias: None,
            ordinal: None,
            axis: "row".into(),
            hidden: false,
            order_by: None,
            sql: None,
            calculation_field_type: None,
            calculation_data_type: None,
            dimension_refs: vec![],
            measure_refs: vec![],
            projection_refs: vec![],
            format: None,
            decorators: vec![],
            parent: None,
            is_static: false,
            source_ref: None,
        };
        ResolvedProjection {
            pid: next_pid(),
            def,
            field: Some(field),
            field_refs: vec![],
            is_static: false,
            source: RefCell::new(None),
            persisted: false,
            links: vec![],
            carried_from: None,
        }
    }

    /// An engine-made calculation over a measure, as the inclusion visitor
    /// builds for its roll-up. Rails never validates it, so it classifies as
    /// neither measure nor dimension.
    pub fn for_calc(
        sql: String,
        alias: String,
        ordinal: Option<i64>,
        measure_refs: Vec<&'a Field>,
        dimension_refs: Vec<&'a Field>,
    ) -> Self {
        Self::for_calc_typed(sql, alias, ordinal, measure_refs, dimension_refs, None, false)
    }

    /// Rails `Calculation.static`: an engine calculation with an explicit
    /// classification, hidden, never persisted.
    pub fn for_calc_typed(
        sql: String,
        alias: String,
        ordinal: Option<i64>,
        measure_refs: Vec<&'a Field>,
        dimension_refs: Vec<&'a Field>,
        field_type: Option<&str>,
        is_static: bool,
    ) -> Self {
        let mut def = Projection {
            reference: String::new(),
            kind: zsql_spec::ProjectionKind::Calculation,
            field_uid: None,
            field_kind: None,
            alias: Some(alias),
            ordinal,
            axis: "row".into(),
            hidden: true,
            order_by: None,
            sql: Some(sql),
            calculation_field_type: field_type.map(str::to_string),
            calculation_data_type: Some("decimal".into()),
            dimension_refs: dimension_refs.iter().map(|f| f.uid.clone()).collect(),
            measure_refs: measure_refs.iter().map(|f| f.uid.clone()).collect(),
            projection_refs: vec![],
            format: None,
            decorators: vec![],
            parent: None,
            is_static,
            source_ref: None,
        };
        def.projection_refs = vec![];
        let mut field_refs = dimension_refs;
        field_refs.extend(measure_refs);
        ResolvedProjection {
            pid: next_pid(),
            def,
            field: None,
            field_refs,
            is_static,
            source: RefCell::new(None),
            persisted: false,
            links: vec![],
            carried_from: None,
        }
    }

    pub fn source(&self) -> Option<Rc<ResolvedProjection<'a>>> {
        self.source.borrow().clone()
    }

    pub fn set_source(&self, s: Option<Rc<ResolvedProjection<'a>>>) {
        *self.source.borrow_mut() = s;
    }

    /// A copy sharing the pid, so the seat and identity of a static stay
    /// while provenance or decorators are set on it.
    pub fn clone_shallow(&self) -> Self {
        ResolvedProjection {
            pid: self.pid,
            def: self.def.clone(),
            field: self.field,
            field_refs: self.field_refs.clone(),
            is_static: self.is_static,
            source: RefCell::new(self.source()),
            persisted: self.persisted,
            links: self.links.clone(),
            carried_from: self.carried_from,
        }
    }

    /// Rails `Calculation#has_aggregate_functions?` over the raw formula.
    pub fn has_aggregate_functions(&self, aggregates: &[String]) -> bool {
        let Some(sql) = &self.def.sql else { return false };
        let refs: Vec<&str> = aggregates.iter().map(String::as_str).collect();
        crate::sql::expr::has_aggregates(sql, &refs)
    }

    pub fn is_calculation(&self) -> bool {
        self.def.is_calculation()
    }

    pub fn is_carried(&self) -> bool {
        self.def.is_carried()
    }

    pub fn is_measure(&self) -> bool {
        if self.is_calculation() {
            self.def.calculation_field_type.as_deref() == Some("measure")
        } else {
            self.field.map(|f| f.is_measure()).unwrap_or(false)
        }
    }

    pub fn is_dimension(&self) -> bool {
        if self.is_calculation() {
            self.def.calculation_field_type.as_deref() == Some("dimension")
        } else {
            self.field.map(|f| f.is_dimension()).unwrap_or(false)
        }
    }

    pub fn field_id(&self) -> Option<Id> {
        self.field.map(|f| f.id)
    }

    /// Rails `Projection#alias`: explicit alias, else the alias a decorator
    /// lends, else the field name.
    pub fn alias(&self) -> String {
        if let Some(a) = &self.def.alias {
            return a.clone();
        }
        let name = self.field.map(|f| f.name.as_str()).unwrap_or("");
        let mut lent: Vec<(i32, String)> = self.def.decorators.iter().filter_map(|d| d.alias(name).map(|a| (d.priority(), a))).collect();
        if !lent.is_empty() {
            lent.sort_by_key(|(p, _)| *p);
            return lent.pop().unwrap().1;
        }
        name.to_string()
    }

    /// Rails `Projection#to_s` / `Calculation#to_s`; part of node identity.
    pub fn identity(&self) -> String {
        if self.is_calculation() {
            format!("<calc:{}:{}>", self.alias(), self.def.sql.clone().unwrap_or_default())
        } else {
            format!("<proj:{}>", self.field.map(field_identity).unwrap_or_default())
        }
    }

    /// What makes two projections one column: the field shaped the same way.
    pub fn column_key(&self) -> String {
        if self.is_calculation() {
            let decs: Vec<_> = sorted_decorators(&self.def.decorators).iter().map(|d| d.column_key()).collect();
            return serde_json::json!(["calc", self.def.sql, decs]).to_string();
        }
        let decs: Vec<_> = sorted_decorators(&self.def.decorators).iter().map(|d| d.column_key()).collect();
        serde_json::json!([self.def.field_uid, decs]).to_string()
    }

    pub fn match_alias(&self, other: &str) -> bool {
        self.alias().trim().to_lowercase() == other.trim().to_lowercase()
    }

    /// Rails `Calculation#measure_dependent?`: stands on a measure by
    /// classification, by @m reference, or through a projected measure's alias.
    pub fn measure_dependent(&self, projections: &[Rc<ResolvedProjection<'a>>]) -> bool {
        self.is_measure()
            || !self.def.measure_refs.is_empty()
            || self.def.projection_refs.iter().any(|a| projections.iter().any(|p| p.match_alias(a) && p.is_measure()))
    }
}

pub fn sorted_decorators(decs: &[Decorator]) -> Vec<&Decorator> {
    let mut v: Vec<&Decorator> = decs.iter().collect();
    v.sort_by_key(|d| d.priority());
    v
}

/// Rails `Semantic::Field#to_s`: `[Name]@D` / `[Name]@M`.
pub fn field_identity(f: &Field) -> String {
    let k = match f.kind {
        FieldKind::Dimension => "D",
        FieldKind::Measure => "M",
    };
    format!("[{}]@{}", f.name, k)
}

/// Rails `Semantic::Table#to_s`: `[datasource_uid:table_uid]`.
pub fn table_identity(semantic: &SemanticModel, t: &Table) -> String {
    let ds = semantic.datasource(t.datasource_id).map(|d| d.uid.as_str()).unwrap_or("");
    format!("[{}:{}]", ds, t.uid)
}

#[derive(Debug)]
pub struct ResolvedFilter<'a> {
    pub fid: usize,
    pub def: Filter,
    pub field: &'a Field,
    /// The fields a comparison filter compares `field` with: the value
    /// field, then `between`'s upper bound. Empty for a filter on a value.
    pub others: Vec<&'a Field>,
}

impl<'a> ResolvedFilter<'a> {
    /// A filter on a value; a comparison resolves through `resolve`.
    pub fn new(fid: usize, def: Filter, field: &'a Field) -> Self {
        ResolvedFilter { fid, def, field, others: vec![] }
    }

    /// The filter with its comparison operands looked up in the model.
    pub fn resolve(semantic: &'a SemanticModel, fid: usize, def: Filter, field: &'a Field) -> Result<Self> {
        let mut others = Vec::new();
        for o in def.value_field.iter().chain(def.value_end_field.iter()) {
            let f = semantic.field_by_uid(&o.field_uid, Some(o.field_kind)).ok_or_else(|| {
                Error::Resolution(format!("Query is not valid: [\"Filters Field {} is no longer available in this branch\"]", o.field_uid))
            })?;
            others.push(f);
        }
        Ok(ResolvedFilter { fid, def, field, others })
    }

    /// Whether `field` is compared with other fields rather than a value.
    /// A measure on either side is `field`, so `is_measure` covers both.
    pub fn is_comparison(&self) -> bool {
        !self.others.is_empty()
    }
    /// Every field the filter reads: its own, then the ones compared with.
    pub fn fields(&self) -> impl Iterator<Item = &'a Field> + '_ {
        std::iter::once(self.field).chain(self.others.iter().copied())
    }
    pub fn is_measure(&self) -> bool {
        self.field.is_measure()
    }
    pub fn is_dimension(&self) -> bool {
        self.field.is_dimension()
    }
    /// A filter on a date value. A comparison of two dates is not one:
    /// there is no literal to widen, apply greedily or match to a partition.
    pub fn is_date(&self) -> bool {
        self.field.is_date() && !self.is_comparison()
    }
}

/// Hints name tables, by uid or by name. One that resolves to nothing is a
/// mistake rather than a preference, so it refuses instead of going quiet;
/// one that resolves to two tables is the same mistake with more than one
/// way to read it, and says both.
fn resolve_hints<'a>(semantic: &'a SemanticModel, hints: &[String], whose: &dyn Fn() -> String) -> Result<Vec<&'a Table>> {
    let mut out = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    for h in hints {
        match semantic.tables_by_uid_or_name(h).as_slice() {
            [one] => out.push(*one),
            [] => unknown.push(format!("{h}{}", did_you_mean(semantic, h))),
            several => {
                let names: Vec<&str> = several.iter().map(|t| t.uid.as_str()).collect();
                return Err(Error::Resolution(format!(
                    "{} hint {h} names {} tables; say which by uid: {}",
                    whose(),
                    several.len(),
                    names.join(", ")
                )));
            }
        }
    }
    if !unknown.is_empty() {
        return Err(Error::Resolution(format!("{} hints name no table: {}", whose(), unknown.join("; "))));
    }
    Ok(out)
}

/// The nearest table names to a term, for a refusal. Both sides are
/// normalized first, as every other near-match in the system is: without
/// that, `web sails` scores below the floor against `web_sales` and the
/// hint spelling this resolver exists to accept gets no suggestion.
fn did_you_mean(semantic: &SemanticModel, term: &str) -> String {
    let want = zsql_spec::fuzzy::normalize(term);
    if want.chars().count() < zsql_spec::fuzzy::MIN_LEN {
        return String::new();
    }
    let mut near: Vec<(f64, &str)> = semantic
        .tables()
        .iter()
        .flat_map(|t| {
            let best = zsql_spec::fuzzy::similarity(&want, &zsql_spec::fuzzy::normalize(&t.name))
                .max(zsql_spec::fuzzy::similarity(&want, &zsql_spec::fuzzy::normalize(&t.uid)));
            (best >= zsql_spec::fuzzy::FLOOR).then_some((best, t.name.as_str()))
        })
        .collect();
    near.sort_by(|a, b| b.0.total_cmp(&a.0));
    near.dedup_by(|a, b| a.1 == b.1);
    let names: Vec<&str> = near.iter().take(zsql_spec::fuzzy::SUGGESTIONS).map(|(_, n)| *n).collect();
    if names.is_empty() {
        String::new()
    } else {
        format!(" (did you mean {})", names.join(", "))
    }
}

#[derive(Debug)]
pub struct ResolvedQuery<'a> {
    pub def: Query,
    pub projections: Vec<Rc<ResolvedProjection<'a>>>,
    pub filters: Vec<Rc<ResolvedFilter<'a>>>,
    pub hints: Vec<&'a Table>,
    /// The tree over filter fids, defaulting to a flat AND of every filter.
    pub tree: FilledTree,
    /// Static calculations visitors append to the query while planning
    /// (Rails pushes them onto the association target).
    pub extras: std::cell::RefCell<Vec<Rc<ResolvedProjection<'a>>>>,
    /// Segments the queryable can see, in creation order (sid = index).
    pub segments: Vec<Rc<ResolvedSegment<'a>>>,
}

/// A segment bound to the universe: its own projections, filters and tree,
/// and where it is applied.
#[derive(Debug)]
pub struct ResolvedSegment<'a> {
    pub sid: usize,
    pub def: zsql_spec::Segment,
    pub projections: Vec<Rc<ResolvedProjection<'a>>>,
    pub filters: Vec<Rc<ResolvedFilter<'a>>>,
    pub tree: FilledTree,
    /// Whether the raw tree is anything but a flat AND (node identity).
    pub custom_tree: bool,
    /// Links: (mode, target pid or None for the whole queryable), in creation order.
    pub links: Vec<(LinkMode, Option<usize>, i64)>,
    pub hints: Vec<&'a Table>,
}

impl<'a> ResolvedSegment<'a> {
    pub fn name(&self) -> String {
        self.def.name.clone().unwrap_or_default()
    }

    /// Rails `Segment#key_dimensions`: projected dimensions that are neither
    /// calculations nor expansions.
    pub fn key_dimensions(&self) -> Vec<&'a Field> {
        self.projections
            .iter()
            .filter(|p| p.is_dimension() && !p.is_calculation() && !p.def.is_expansion())
            .filter_map(|p| p.field)
            .collect()
    }

    /// The query this segment plans as, for its population.
    pub fn as_query(&self) -> ResolvedQuery<'a> {
        ResolvedQuery {
            def: zsql_spec::Query {
                population: true,
                name: self.def.name.clone(),
                limit: self.def.limit,
                db_settings: self.def.db_settings.clone(),
                hints: self.def.hints.clone(),
                filter_tree: self.def.filter_tree.clone(),
                projections: vec![],
                filters: vec![],
                segments: vec![],
                corrections: vec![],
            },
            projections: self.projections.clone(),
            filters: self.filters.clone(),
            hints: self.hints.clone(),
            tree: self.tree.clone(),
            extras: Default::default(),
            segments: vec![],
        }
    }
}

/// A filter tree with leaves resolved to filter ids.
#[derive(Debug, Clone, PartialEq)]
pub enum FilledTree {
    Group(String, Vec<FilledTree>),
    Leaf(usize),
}

impl FilledTree {
    fn from_tree(t: &Tree, by_ref: &std::collections::HashMap<&str, usize>) -> Option<FilledTree> {
        match t {
            Tree::Leaf(r) => by_ref.get(r.as_str()).map(|&f| FilledTree::Leaf(f)),
            Tree::Group(op, kids) => {
                // Rails swaps unknown ids to nil and compacts them away.
                let kids: Vec<_> = kids.iter().filter_map(|k| FilledTree::from_tree(k, by_ref)).collect();
                Some(FilledTree::Group(op.clone(), kids))
            }
        }
    }

    /// Whether the tree is anything but a flat AND.
    pub fn custom(&self) -> bool {
        match self {
            FilledTree::Group(op, kids) => op != "and" || kids.iter().any(|k| matches!(k, FilledTree::Group(..))),
            FilledTree::Leaf(_) => false,
        }
    }
}

impl<'a> ResolvedQuery<'a> {
    pub fn resolve(semantic: &'a SemanticModel, def: Query) -> Result<Self> {
        let mut projections: Vec<ResolvedProjection<'a>> = Vec::new();
        for p in &def.projections {
            let field = match (&p.field_uid, p.is_calculation()) {
                (Some(uid), false) => Some(semantic.field_by_uid(uid, p.field_kind).ok_or_else(|| {
                    Error::Resolution(format!("Query is not valid: [\"Projections Field {uid} is no longer available in this branch\"]"))
                })?),
                _ => None,
            };
            for d in &p.decorators {
                if d.kind == DecoratorKind::Truncate {
                    let Some(f) = field else { continue };
                    if !f.is_date() {
                        return Err(Error::Resolution("Query is not valid: only date/datetime types can be date truncated".into()));
                    }
                    let grains: Vec<&str> = if f.grains.is_empty() {
                        vec!["day", "month", "week", "quarter", "year"]
                    } else {
                        f.grains.iter().map(String::as_str).collect()
                    };
                    if let Some(g) = d.grain() {
                        if !grains.contains(&g) {
                            return Err(Error::Resolution(format!(
                                "Query is not valid: Decorators has errors ({g} is not an enabled date grain for {})",
                                f.name
                            )));
                        }
                    }
                }
            }
            let mut field_refs = Vec::new();
            for uid in &p.dimension_refs {
                if let Some(f) = semantic.field_by_uid(uid, Some(FieldKind::Dimension)) {
                    field_refs.push(f);
                }
            }
            for uid in &p.measure_refs {
                if let Some(f) = semantic.field_by_uid(uid, Some(FieldKind::Measure)) {
                    field_refs.push(f);
                }
            }
            let is_static = p.is_static;
            projections.push(ResolvedProjection {
                pid: next_pid(),
                def: p.clone(),
                field,
                field_refs,
                is_static,
                source: RefCell::new(None),
                persisted: !is_static,
                links: vec![],
                carried_from: None,
            });
        }
        let mut filters = Vec::new();
        let mut by_ref = std::collections::HashMap::new();
        for (i, f) in def.filters.iter().enumerate() {
            let field = semantic.field_by_uid(&f.field_uid, f.field_kind).ok_or_else(|| {
                Error::Resolution(format!("Query is not valid: [\"Filters Field {} is no longer available in this branch\"]", f.field_uid))
            })?;
            by_ref.insert(f.reference.as_str(), i);
            filters.push(Rc::new(ResolvedFilter::resolve(semantic, i, f.clone(), field)?));
        }
        let tree = match &def.filter_tree {
            Some(t) => FilledTree::from_tree(t, &by_ref).unwrap_or(FilledTree::Group("and".into(), vec![])),
            None => FilledTree::Group("and".into(), (0..filters.len()).map(FilledTree::Leaf).collect()),
        };
        let hints = resolve_hints(semantic, &def.hints, &|| "the query's".to_string())?;

        // Segments: their own components, then links resolved onto the
        // query's projections and carried columns onto their definitions.
        let mut fid = filters.len();
        let mut segments: Vec<ResolvedSegment<'a>> = Vec::new();
        for (sid, s) in def.segments.iter().enumerate() {
            let mut sprojs = Vec::new();
            for p in &s.projections {
                let field = match (&p.field_uid, p.is_calculation()) {
                    (Some(uid), false) => semantic.field_by_uid(uid, p.field_kind),
                    _ => None,
                };
                let mut field_refs = Vec::new();
                for uid in &p.dimension_refs {
                    if let Some(f) = semantic.field_by_uid(uid, Some(FieldKind::Dimension)) {
                        field_refs.push(f);
                    }
                }
                for uid in &p.measure_refs {
                    if let Some(f) = semantic.field_by_uid(uid, Some(FieldKind::Measure)) {
                        field_refs.push(f);
                    }
                }
                sprojs.push(Rc::new(ResolvedProjection {
                    pid: next_pid(),
                    def: p.clone(),
                    field,
                    field_refs,
                    is_static: false,
                    source: RefCell::new(None),
                    persisted: true,
                    links: vec![],
                    carried_from: None,
                }));
            }
            let mut sfilters: Vec<Rc<ResolvedFilter<'a>>> = Vec::new();
            let mut sby_ref = std::collections::HashMap::new();
            for (q_ref, &q_fid) in &by_ref {
                sby_ref.insert(*q_ref, q_fid);
            }
            for f in &s.filters {
                let field = semantic.field_by_uid(&f.field_uid, f.field_kind).ok_or_else(|| {
                    Error::Resolution(format!("Query is not valid: [\"Segment filter field {} is no longer available\"]", f.field_uid))
                })?;
                sby_ref.insert(f.reference.as_str(), fid);
                sfilters.push(Rc::new(ResolvedFilter::resolve(semantic, fid, f.clone(), field)?));
                fid += 1;
            }
            let (stree, custom) = match &s.filter_tree {
                Some(t) => {
                    let filled = FilledTree::from_tree(t, &sby_ref).unwrap_or(FilledTree::Group("and".into(), vec![]));
                    let custom = filled.custom();
                    (filled, custom)
                }
                None => (FilledTree::Group("and".into(), sfilters.iter().map(|f| FilledTree::Leaf(f.fid)).collect()), false),
            };
            let links = s
                .links
                .iter()
                .map(|l| {
                    let target = match &l.target {
                        LinkTarget::Query => None,
                        LinkTarget::Projection(r) => projections.iter().find(|p| p.def.reference == *r).map(|p| p.pid),
                    };
                    (l.mode, target, l.rank)
                })
                .collect();
            // Named lazily: the overwhelming majority of segments have no
            // hints at all, and this is per segment per query.
            let hints = resolve_hints(semantic, &s.hints, &|| match s.name.as_deref() {
                Some(name) => format!("segment '{name}'"),
                None => "the segment's".to_string(),
            })?;
            segments.push(ResolvedSegment {
                sid,
                def: s.clone(),
                projections: sprojs,
                filters: sfilters,
                tree: stree,
                custom_tree: custom,
                links,
                hints,
            });
        }
        // Measure-level links and carried columns onto the query's projections.
        for p in projections.iter_mut() {
            for s in &segments {
                for (mode, target, _) in &s.links {
                    if *target == Some(p.pid) {
                        p.links.push((s.sid, *mode));
                    }
                }
            }
            if let Some(parent_ref) = &p.def.parent {
                for s in &segments {
                    if let Some(def_p) = s.projections.iter().find(|x| x.def.reference == *parent_ref) {
                        p.carried_from = Some((def_p.pid, s.sid));
                    }
                }
            }
        }
        let projections: Vec<Rc<ResolvedProjection<'a>>> = projections.into_iter().map(Rc::new).collect();
        let segments = segments.into_iter().map(Rc::new).collect();
        Ok(ResolvedQuery { def, projections, filters, hints, tree, extras: Default::default(), segments })
    }

    /// Whether this is a segment planned for its own population.
    pub fn is_segment(&self) -> bool {
        self.def.population
    }

    pub fn segment(&self, sid: usize) -> Option<&Rc<ResolvedSegment<'a>>> {
        self.segments.iter().find(|s| s.sid == sid)
    }

    /// Rails `Segmentation.query_level_links`: links on the queryable itself.
    pub fn query_level_links(&self) -> Vec<(usize, LinkMode)> {
        if self.is_segment() {
            return vec![];
        }
        let mut out = Vec::new();
        for s in &self.segments {
            for (mode, target, _) in &s.links {
                if target.is_none() {
                    out.push((s.sid, *mode));
                }
            }
        }
        out
    }

    pub fn query_level_key_dimensions(&self) -> Vec<&'a Field> {
        let mut out: Vec<&Field> = Vec::new();
        for (sid, _) in self.query_level_links() {
            if let Some(s) = self.segment(sid) {
                for f in s.key_dimensions() {
                    if !out.iter().any(|x| x.id == f.id) {
                        out.push(f);
                    }
                }
            }
        }
        out
    }

    /// The query's projections as Rails sees them mid-plan: its own plus
    /// the statics appended so far.
    pub fn all_projections(&self) -> Vec<Rc<ResolvedProjection<'a>>> {
        let mut v = self.projections.clone();
        v.extend(self.extras.borrow().iter().cloned());
        v
    }

    pub fn push_static(&self, calc: Rc<ResolvedProjection<'a>>) {
        self.extras.borrow_mut().push(calc);
    }

    /// Rails `Query.static_calc`: a measure calculation over the formula,
    /// with its `[Name]@m`, `[Name]@d` and `[Alias]` references resolved,
    /// seated after the query's projections and appended to them.
    pub fn static_calc(&self, semantic: &'a SemanticModel, alias: &str, sql: &str) -> Rc<ResolvedProjection<'a>> {
        let (measures, dims, projs) = crate::sql::expr::parse_refs(sql);
        let measure_refs: Vec<&Field> = measures.iter().filter_map(|n| semantic.field_by_name(n, FieldKind::Measure)).collect();
        let dimension_refs: Vec<&Field> = dims.iter().filter_map(|n| semantic.field_by_name(n, FieldKind::Dimension)).collect();
        let mut rp = ResolvedProjection::for_calc_typed(
            sql.to_string(),
            alias.to_string(),
            Some(self.next_ordinal()),
            measure_refs,
            dimension_refs,
            Some("measure"),
            true,
        );
        rp.def.projection_refs = projs;
        let calc = Rc::new(rp);
        self.push_static(calc.clone());
        calc
    }

    /// Rails `Queryable#measures?`: a directly aggregated measure, for node typing.
    pub fn has_measures(&self) -> bool {
        self.projections.iter().any(|p| p.is_measure() && !p.is_static) || self.filters.iter().any(|f| f.is_measure())
    }

    pub fn calculations_with_measures(&self) -> bool {
        self.projections.iter().filter(|p| p.is_calculation()).any(|p| p.field_refs.iter().any(|f| f.is_measure()))
    }

    /// Rails `Ordinalize`: a projection built against the loaded query seats
    /// itself one past its highest peer.
    pub fn next_ordinal(&self) -> i64 {
        self.all_projections().iter().filter_map(|p| p.def.ordinal).max().map(|m| m + 1).unwrap_or(0)
    }

    pub fn db_setting_bool(&self, key: &str) -> bool {
        self.def.db_settings.get(key).and_then(serde_json::Value::as_bool).unwrap_or(false)
    }
}

/// Refuses, as unimplemented, queries that need visitors not yet ported.
pub fn check_supported(q: &ResolvedQuery, semantic: &SemanticModel, principal: Option<&SecurityContext>) -> Result<()> {
    for p in &q.projections {
        for d in &p.def.decorators {
            match d.kind {
                DecoratorKind::Truncate | DecoratorKind::Extract | DecoratorKind::Window | DecoratorKind::Customize => {}
                DecoratorKind::Contribute | DecoratorKind::Temporalize => {}
            }
        }
        for f in &p.field_refs {
            if f.exclusion_type.is_some() || f.inclusions.is_some() {
                return Err(Error::Unimplemented("calculations over rule-bearing measures"));
            }
        }
    }
    comparison_grain(&q.projections, &q.filters, "query")?;
    // A segment's measures define membership at its keys' grain, a
    // separate plan; comparing them there is not built yet.
    for seg in &q.segments {
        if let Some(f) = seg.filters.iter().find(|f| f.is_measure() && f.is_comparison()) {
            return Err(Error::Resolution(format!(
                "Segment {} compares the measure {} with another field; a segment's filters compare dimensions only.",
                seg.name(),
                f.field.name
            )));
        }
    }
    let _ = (principal, semantic);
    Ok(())
}

/// A measure compares with a dimension at the grain it is aggregated to,
/// so the dimension has to be part of that grain: projected as it is.
/// Otherwise the comparison is a row-level question about the measure's
/// inputs, which a calculated dimension asks, not a filter.
fn comparison_grain(projections: &[Rc<ResolvedProjection>], filters: &[Rc<ResolvedFilter>], whose: &str) -> Result<()> {
    for f in filters.iter().filter(|f| f.is_measure() && f.is_comparison()) {
        for d in f.others.iter().filter(|d| d.is_dimension()) {
            let grouped = projections
                .iter()
                .any(|p| !p.is_calculation() && !p.def.is_expansion() && p.field_id() == Some(d.id) && p.def.decorators.is_empty());
            if !grouped {
                return Err(Error::Resolution(format!(
                    "The filter comparing {} with {} needs {} among the {whose}'s dimensions, undecorated: a measure is compared with a dimension at the grain it is aggregated to.",
                    f.field.name, d.name, d.name
                )));
            }
        }
    }
    Ok(())
}

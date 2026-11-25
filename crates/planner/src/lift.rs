//! A calculation that is a measure defined on dimensions, such as
//! `count(distinct [Customer ID]@d)`, is planned as the model measure it
//! would be had the modeler declared it: a field, an expression on each
//! table it can be aggregated on, and a path that roots it there. The
//! records are made for the one query, in a copy of the model, and the
//! calculation is rewritten into a projection of the field. Everything
//! after that sees a measure: the resolver hosts it in one universe, and
//! segments, prior periods, shares, windows and top-n rankings treat it as
//! they treat any other.
//!
//! A model measure is declared on one table. A calculation has no home, so
//! it is mapped on every table that reaches all the dimensions it reads,
//! and the resolver's ranking picks among them: a universe the query
//! already aggregates in comes first.

use crate::{Error, Result};
use serde_json::json;
use zsql_semantic::{Expression, Field, FieldKind, Id, Path, SemanticModel, Snapshot};
use zsql_spec::{Projection, ProjectionKind, Query};

/// The model with the lifted measures in it, the paths that reach them,
/// and the query that projects them.
pub struct Lifted {
    pub snapshot: Snapshot,
    pub paths: Vec<Path>,
    pub query: Query,
}

/// `None` when the query has nothing to lift.
pub fn lift(semantic: &SemanticModel, paths: &[Path], aggregates: &[String], query: &Query) -> Result<Option<Lifted>> {
    let projected: Vec<&str> = query
        .projections
        .iter()
        .filter(|p| p.kind == ProjectionKind::Field && p.field_kind == Some(FieldKind::Dimension))
        .filter_map(|p| p.field_uid.as_deref())
        .collect();
    for p in &query.projections {
        refuse_measures_with_unprojected_dimensions(semantic, aggregates, query, p, &projected)?;
    }
    let wanted: Vec<usize> = query.projections.iter().enumerate().filter(|(_, p)| liftable(p, &projected)).map(|(i, _)| i).collect();
    if wanted.is_empty() {
        return Ok(None);
    }

    let snap = semantic.snapshot();
    let mut next_id = [
        snap.datasources.iter().map(|x| x.id).max(),
        snap.tables.iter().map(|x| x.id).max(),
        snap.fields.iter().map(|x| x.id).max(),
        snap.expressions.iter().map(|x| x.id).max(),
        snap.join_defs.iter().map(|x| x.id).max(),
        snap.partitions.iter().map(|x| x.id).max(),
        snap.paths.iter().map(|x| x.id).max(),
        paths.iter().map(|x| x.id).max(),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(0);
    let mut fresh = || {
        next_id += 1;
        next_id
    };

    let mut snapshot = snap.clone();
    let mut all_paths = paths.to_vec();
    let mut query = query.clone();
    for (n, i) in wanted.into_iter().enumerate() {
        let calc = query.projections[i].clone();
        let alias = calc.alias.clone().unwrap_or_default();
        let dims: Vec<&Field> = calc.dimension_refs.iter().filter_map(|u| semantic.field_by_uid(u, Some(FieldKind::Dimension))).collect();
        // Every table whose universe reaches all of them.
        let tables: Vec<Id> = snap
            .tables
            .iter()
            .filter(|t| dims.iter().all(|d| paths.iter().any(|p| p.root_table_id == t.id && p.field_id == d.id)))
            .map(|t| t.id)
            .collect();
        if tables.is_empty() {
            let names: Vec<&str> = dims.iter().map(|d| d.name.as_str()).collect();
            return Err(Error::Resolution(format!(
                "Calculation \"{alias}\" aggregates {}, which no table reaches together, so it cannot be computed.",
                names.join(", ")
            )));
        }

        // The name is what `[Name]@m` and the engine's own columns go by,
        // so it must not answer to a field the model already has.
        let taken = |name: &str| {
            semantic.field_by_name(name, FieldKind::Measure).is_some()
                || semantic.field_by_name(name, FieldKind::Dimension).is_some()
                || snapshot.fields.iter().any(|f| f.name.eq_ignore_ascii_case(name))
        };
        let name = if taken(&alias) { format!("{alias} (calculation {})", n + 1) } else { alias.clone() };
        let field_id = fresh();
        let uid = format!("calculation-{field_id}");
        let field: Field = serde_json::from_value(json!({
            "id": field_id,
            "uid": uid,
            "name": name,
            "type": "Semantic::Measure",
            "data_type": calc.calculation_data_type.clone().unwrap_or_else(|| "decimal".into()),
        }))
        .map_err(|e| Error::Resolution(format!("Calculation \"{alias}\" could not be planned as a measure: {e}")))?;
        snapshot.fields.push(field);

        // The formula as a model expression writes it: references by name.
        let sql = calc.sql.clone().unwrap_or_default().replace("]@d", "]").replace("]@D", "]");
        let dim_ids: Vec<Id> = dims.iter().map(|d| d.id).collect();
        for table_id in tables {
            let expression: Expression = serde_json::from_value(json!({
                "id": fresh(),
                "table_id": table_id,
                "field_id": field_id,
                "sql": sql,
                "dimension_ref_ids": dim_ids,
            }))
            .map_err(|e| Error::Resolution(format!("Calculation \"{alias}\" could not be planned as a measure: {e}")))?;
            snapshot.expressions.push(expression);
            let table = snap.tables.iter().find(|t| t.id == table_id).expect("table");
            let path: Path = serde_json::from_value(json!({
                "id": fresh(),
                "root_table_id": table_id,
                "target_table_id": table_id,
                "from_table_id": null,
                "field_id": field_id,
                "field_type": "Semantic::Measure",
                "step_number": 0,
                "cost": table.cost,
                "tier": 0,
                "joins": [],
                "table_nodes": [table_id],
                "datasource_id": table.datasource_id,
                "blending_field_id": null,
                "extended_blend_group": null,
            }))
            .map_err(|e| Error::Resolution(format!("Calculation \"{alias}\" could not be planned as a measure: {e}")))?;
            all_paths.push(path);
        }

        let p = &mut query.projections[i];
        p.kind = ProjectionKind::Field;
        p.field_uid = Some(uid);
        p.field_kind = Some(FieldKind::Measure);
        p.sql = None;
        p.calculation_field_type = None;
        p.dimension_refs = vec![];
        // Kept set: the resolver reads it to aggregate the measure in one
        // universe, since unlike a model measure it is mapped on many.
    }
    Ok(Some(Lifted { snapshot, paths: all_paths, query }))
}

/// One that reads only projected dimensions is already at the query's
/// grain wherever it is evaluated, so it stays a calculation; so does one
/// that reads a projection by alias.
fn liftable(p: &Projection, projected: &[&str]) -> bool {
    p.kind == ProjectionKind::Calculation
        && p.aggregates_dimensions
        && p.projection_refs.is_empty()
        && p.dimension_refs.iter().any(|u| !projected.contains(&u.as_str()))
}

/// A calculation that reads a measure may read a dimension only when the
/// query projects it: the measure is then evaluated at the query's grain
/// and the dimension is a value of the row. One the query does not project
/// would have to be folded into every aggregation's grain to be read at
/// the merge, and the measures beside the calculation would be aggregated
/// at the wrong grain. That is refused, with what to write instead.
fn refuse_measures_with_unprojected_dimensions(
    semantic: &SemanticModel,
    aggregates: &[String],
    query: &Query,
    p: &Projection,
    projected: &[&str],
) -> Result<()> {
    if p.kind != ProjectionKind::Calculation || p.calculation_field_type.as_deref() != Some("measure") {
        return Ok(());
    }
    let unprojected: Vec<&Field> = p
        .dimension_refs
        .iter()
        .filter(|u| !projected.contains(&u.as_str()))
        .filter_map(|u| semantic.field_by_uid(u, Some(FieldKind::Dimension)))
        .collect();
    if unprojected.is_empty() {
        return Ok(());
    }
    // Measures it reads by `@m`, and by the alias of a measure projection.
    let is_measure = |q: &Projection| match q.kind {
        ProjectionKind::Field => q.field_kind == Some(FieldKind::Measure),
        ProjectionKind::Calculation => q.calculation_field_type.as_deref() == Some("measure"),
        ProjectionKind::Expansion => false,
    };
    let measure_aliases: Vec<String> = p
        .projection_refs
        .iter()
        .filter(|r| {
            query
                .projections
                .iter()
                .any(|q| is_measure(q) && q.alias.as_deref().map(|a| a.trim().eq_ignore_ascii_case(r.trim())).unwrap_or(false))
        })
        .map(|r| r.trim().to_lowercase())
        .collect();
    let mut measures: Vec<String> =
        p.measure_refs.iter().filter_map(|u| semantic.field_by_uid(u, Some(FieldKind::Measure))).map(|f| f.name.clone()).collect();
    measures.extend(p.projection_refs.iter().filter(|r| measure_aliases.contains(&r.trim().to_lowercase())).cloned());
    if measures.is_empty() {
        return Ok(());
    }

    let alias = p.alias.clone().unwrap_or_default();
    let dims = unprojected.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ");
    let names: Vec<&str> = aggregates.iter().map(String::as_str).collect();
    let sql = p.sql.as_deref().unwrap_or_default();
    let together = zsql_spec::expr::aggregate_calls(sql, &names).into_iter().any(|c| {
        (!c.measures.is_empty() || c.aliases.iter().any(|a| measure_aliases.contains(a)))
            && c.dimensions.iter().any(|d| unprojected.iter().any(|u| u.name.eq_ignore_ascii_case(d)))
    });
    Err(Error::Resolution(if together {
        format!(
            "Calculation \"{alias}\" reads the measure {} inside an aggregate over {dims}, which the query does not project. A measure is already an aggregate, so it would first have to be totalled per {dims}, and that is not planned yet. Project {dims} to evaluate the measure at that grain, or aggregate {dims} alone in the calculation and apply a segment keyed on {dims} that filters on the measure.",
            measures.join(", ")
        )
    } else {
        format!(
            "Calculation \"{alias}\" reads the measure {} together with {dims}, which the query does not project. Give the aggregate over {dims} a calculation of its own, as calc(count(distinct [{}]@d)) is, and read it here by its alias.",
            measures.join(", "),
            unprojected[0].name
        )
    }))
}

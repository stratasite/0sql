//! A calculation that is a measure defined on dimensions, such as
//! `count(distinct [Customer ID]@d)`, is planned as the model measure it
//! would be had the modeler declared it: a field, an expression on each
//! table it can be aggregated on, and a path that roots it there. The
//! records are made for the one query and laid over the model, which is
//! neither copied nor changed, and the calculation is rewritten into a
//! projection of the field. Everything after that sees a measure: the
//! resolver hosts it in one universe, and segments, prior periods, shares,
//! windows and top-n rankings treat it as they treat any other.
//!
//! A model measure is declared on one table. A calculation has no home, so
//! it is mapped on every table that reaches all the dimensions it reads,
//! and the resolver's ranking picks among them: a universe the query
//! already aggregates in comes first.

use crate::universe::Universes;
use crate::{Error, Result};
use serde_json::json;
use zsql_semantic::{Expression, Field, FieldKind, Id, Path, SemanticModel};
use zsql_spec::{Projection, ProjectionKind, Query};

/// What every synthetic measure's name and uid start with. No spec can
/// name one: lowering resolves a spec against the model before any exists.
pub const SYNTHETIC_MEASURE: &str = "synmsr:";

/// The records to lay over the model and its universes, and the query that
/// projects the synthetic measures in place of the calculations.
pub struct Lifted {
    pub fields: Vec<Field>,
    pub expressions: Vec<Expression>,
    pub paths: Vec<Path>,
    pub query: Query,
}

/// `None` when the query has nothing to lift.
pub fn lift(semantic: &SemanticModel, universes: &Universes, aggregates: &[String], query: &Query) -> Result<Option<Lifted>> {
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

    // Synthetic records count down from -1: a range no model id is in, so
    // none can collide and one is plain to see in an explain.
    let mut next_id: Id = 0;
    let mut fresh = || {
        next_id -= 1;
        next_id
    };
    let invalid =
        |alias: &str, e: serde_json::Error| Error::Resolution(format!("Calculation \"{alias}\" could not be planned as a measure: {e}"));

    let mut lifted = Lifted { fields: vec![], expressions: vec![], paths: vec![], query: query.clone() };
    // (name, formula, data type) of each field made so far.
    let mut made: Vec<(String, String, String)> = Vec::new();
    for i in wanted {
        let calc = lifted.query.projections[i].clone();
        let alias = calc.alias.clone().unwrap_or_default();
        let dims: Vec<&Field> = calc.dimension_refs.iter().filter_map(|u| semantic.field_by_uid(u, Some(FieldKind::Dimension))).collect();
        // The formula as a model expression writes it: references by name.
        let sql = calc.sql.clone().unwrap_or_default().replace("]@d", "]").replace("]@D", "]");
        let data_type = calc.calculation_data_type.clone().unwrap_or_else(|| "decimal".into());

        // Named for the alias, so a message that names the field reads as
        // the calculation. Two projections of one formula under one alias
        // share a field; a name already taken gets a number.
        let mut name = format!("{SYNTHETIC_MEASURE}{alias}");
        let mut n = 1;
        let reuse = loop {
            match made.iter().find(|(made_name, ..)| made_name.eq_ignore_ascii_case(&name)) {
                Some((_, made_sql, made_type)) if *made_sql == sql && *made_type == data_type => break true,
                None if semantic.field_by_name(&name, FieldKind::Measure).is_none() => break false,
                _ => {
                    n += 1;
                    name = format!("{SYNTHETIC_MEASURE}{alias}#{n}");
                }
            }
        };
        let uid = name.to_lowercase();

        if !reuse {
            // Every table whose universe reaches all the dimensions.
            let tables: Vec<_> = universes.bases.iter().filter(|b| dims.iter().all(|d| b.has_dimension(d.id))).map(|b| b.root).collect();
            if tables.is_empty() {
                let names: Vec<&str> = dims.iter().map(|d| d.name.as_str()).collect();
                return Err(Error::Resolution(format!(
                    "Calculation \"{alias}\" aggregates {}, which no table reaches together, so it cannot be computed.",
                    names.join(", ")
                )));
            }
            let field_id = fresh();
            let field: Field = serde_json::from_value(json!({
                "id": field_id,
                "uid": uid,
                "name": name,
                "type": "Semantic::Measure",
                "data_type": data_type,
            }))
            .map_err(|e| invalid(&alias, e))?;
            lifted.fields.push(field);
            made.push((name.clone(), sql.clone(), data_type.clone()));

            let dim_ids: Vec<Id> = dims.iter().map(|d| d.id).collect();
            for table in tables {
                let expression: Expression = serde_json::from_value(json!({
                    "id": fresh(),
                    "table_id": table.id,
                    "field_id": field_id,
                    "sql": sql,
                    "dimension_ref_ids": dim_ids,
                }))
                .map_err(|e| invalid(&alias, e))?;
                lifted.expressions.push(expression);
                // As a measure declared on the table is reached: at the root.
                let path: Path = serde_json::from_value(json!({
                    "id": fresh(),
                    "root_table_id": table.id,
                    "target_table_id": table.id,
                    "from_table_id": null,
                    "field_id": field_id,
                    "field_type": "Semantic::Measure",
                    "step_number": 0,
                    "cost": table.cost,
                    "tier": 0,
                    "joins": [],
                    "table_nodes": [table.id],
                    "datasource_id": table.datasource_id,
                    "blending_field_id": null,
                    "extended_blend_group": null,
                }))
                .map_err(|e| invalid(&alias, e))?;
                lifted.paths.push(path);
            }
        }

        let p = &mut lifted.query.projections[i];
        p.kind = ProjectionKind::Field;
        p.field_uid = Some(uid);
        p.field_kind = Some(FieldKind::Measure);
        p.sql = None;
        p.calculation_field_type = None;
        p.dimension_refs = vec![];
        // `aggregates_dimensions` stays set: the resolver reads it to
        // aggregate the measure in one universe, since unlike a model
        // measure it is mapped on many.
    }
    Ok(Some(lifted))
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

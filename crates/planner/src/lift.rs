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
use zsql_semantic::{DataType, Expression, Field, FieldKind, Id, Path, SemanticModel};
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
    let projected = Projected::of(query);
    let names: Vec<&str> = aggregates.iter().map(String::as_str).collect();
    for p in &query.projections {
        refuse_measures_with_unprojected_dimensions(semantic, &names, query, p, &projected)?;
    }
    let wanted: Vec<usize> =
        query.projections.iter().enumerate().filter(|(_, p)| liftable(p, &projected, &names)).map(|(i, _)| i).collect();
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
    let mut lifted = Lifted { fields: vec![], expressions: vec![], paths: vec![], query: query.clone() };
    // (name, formula, data type) of each field made so far.
    let mut made: Vec<(String, String, String)> = Vec::new();
    for i in wanted {
        let calc = lifted.query.projections[i].clone();
        let alias = calc.alias.clone().unwrap_or_default();
        let dims: Vec<&Field> = calc.dimension_refs.iter().filter_map(|u| semantic.field_by_uid(u, Some(FieldKind::Dimension))).collect();
        let sql = calc.sql.clone().unwrap_or_default();
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
            lifted.fields.push(Field::measure(field_id, uid.clone(), name.clone(), DataType::from_name(&data_type)));
            made.push((name.clone(), sql.clone(), data_type.clone()));

            let dim_ids: Vec<Id> = dims.iter().map(|d| d.id).collect();
            for table in tables {
                lifted.expressions.push(Expression::over_dimensions(fresh(), table.id, field_id, sql.clone(), dim_ids.clone()));
                // As a measure declared on the table is reached: at the root.
                lifted.paths.push(Path::at_root(fresh(), table, field_id, FieldKind::Measure));
            }
        }

        let p = &mut lifted.query.projections[i];
        p.kind = ProjectionKind::Field;
        p.field_uid = Some(uid);
        p.field_kind = Some(FieldKind::Measure);
        p.sql = None;
        p.calculation_field_type = None;
        p.dimension_refs = vec![];
    }
    Ok(Some(lifted))
}

/// The dimensions the query projects, by uid: all of them, and those
/// projected as they are. A truncated or extracted date is a different
/// value from the date, so an aggregate over `[Date]@d` beside a month of
/// it still reads a dimension the query's rows do not carry.
struct Projected<'q> {
    plain: Vec<&'q str>,
    any: Vec<&'q str>,
}

impl<'q> Projected<'q> {
    fn of(query: &'q Query) -> Self {
        let dims = || query.projections.iter().filter(|p| p.kind == ProjectionKind::Field && p.field_kind == Some(FieldKind::Dimension));
        Projected {
            plain: dims().filter(|p| p.decorators.is_empty()).filter_map(|p| p.field_uid.as_deref()).collect(),
            any: dims().filter_map(|p| p.field_uid.as_deref()).collect(),
        }
    }
}

/// A measure defined on dimensions: the calculation aggregates, reads no
/// measure and no projection, and keeps every dimension inside an
/// aggregate call, as `count(distinct [Customer ID]@d)` does. One that
/// reads only dimensions projected as they are is already at the query's
/// grain wherever it is evaluated, so it stays a calculation.
fn liftable(p: &Projection, projected: &Projected, aggregates: &[&str]) -> bool {
    p.kind == ProjectionKind::Calculation
        && p.calculation_field_type.as_deref() == Some("measure")
        && p.measure_refs.is_empty()
        && p.projection_refs.is_empty()
        && p.dimension_refs.iter().any(|u| !projected.plain.contains(&u.as_str()))
        && p.sql.as_deref().map(|sql| zsql_spec::expr::aggregates_every_reference(sql, aggregates)).unwrap_or(false)
}

/// A calculation that reads a measure may read a dimension only when the
/// query projects it: the measure is then evaluated at the query's grain
/// and the dimension is a value of the row. One the query does not project
/// would have to be folded into every aggregation's grain to be read at
/// the merge, and the measures beside the calculation would be aggregated
/// at the wrong grain. That is refused, with what to write instead.
fn refuse_measures_with_unprojected_dimensions(
    semantic: &SemanticModel,
    aggregates: &[&str],
    query: &Query,
    p: &Projection,
    projected: &Projected,
) -> Result<()> {
    if p.kind != ProjectionKind::Calculation || p.calculation_field_type.as_deref() != Some("measure") {
        return Ok(());
    }
    // Not projected at all; or projected only truncated or extracted and
    // read inside an aggregate, where it is the dimension itself that is
    // aggregated. Outside one it is the projected column's value, as in a
    // window's `order by [Date]@d`.
    let sql = p.sql.as_deref().unwrap_or_default();
    let calls = zsql_spec::expr::aggregate_calls(sql, aggregates);
    let unprojected: Vec<&Field> = p
        .dimension_refs
        .iter()
        .filter_map(|u| semantic.field_by_uid(u, Some(FieldKind::Dimension)).map(|f| (u, f)))
        .filter(|(u, f)| {
            !projected.any.contains(&u.as_str())
                || (!projected.plain.contains(&u.as_str())
                    && calls.iter().any(|c| c.dimensions.iter().any(|d| f.name.eq_ignore_ascii_case(d))))
        })
        .map(|(_, f)| f)
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
    let together = calls.iter().any(|c| {
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

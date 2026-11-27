//! Pages of what a source or a dataset holds, sorted and filtered for
//! viewing; neither changes what is kept.

use crate::error::{Error, Result};
use crate::schema::{ident, Column, ColumnType};
use crate::steps::SortKey;
use zsql_datasource::Batches;

/// How a page is sorted and filtered for viewing.
#[derive(Default)]
pub struct View<'a> {
    pub sort: &'a [SortKey],
    pub filter: Option<&'a str>,
}

/// A page of the rows file at `path`, which holds `total` rows with
/// these columns: read straight from the file when the view neither
/// sorts nor filters, only as far as the page; else through the engine.
pub(crate) fn page_of(
    engine: &crate::engine::Engine,
    columns: &[Column],
    (path, total): (&std::path::Path, u64),
    offset: u64,
    limit: u64,
    view: &View<'_>,
) -> Result<(u64, Batches)> {
    if view.sort.is_empty() && view.filter.is_none() {
        let as_usize = |n: u64| usize::try_from(n).unwrap_or(usize::MAX);
        let rows = crate::store::read_rows_range(path, as_usize(offset), as_usize(limit))?;
        // Only when the file holds the columns as the engine would hand
        // them back; anything else is paged as it always was.
        let fields = rows.schema.fields();
        if fields.len() == columns.len() && fields.iter().zip(columns).all(|(f, c)| f.name() == &c.name && *f.data_type() == c.ty.arrow()) {
            return Ok((total, rows));
        }
    }
    let rows = crate::store::read_rows(path)?;
    page(engine, columns, &rows, offset, limit, view)
}

/// A page of rows with these columns, and how many rows the view has.
pub(crate) fn page(
    engine: &crate::engine::Engine,
    columns: &[Column],
    rows: &Batches,
    offset: u64,
    limit: u64,
    view: &View<'_>,
) -> Result<(u64, Batches)> {
    let session = engine.session()?;
    let typed: Vec<(String, ColumnType)> = columns.iter().map(|c| (c.name.clone(), c.ty)).collect();
    session.load("v", &typed, &rows.batches)?;
    let pinned_at = crate::time::now();
    let filter = match view.filter {
        Some(f) => {
            let c = crate::formula::compile(f, &crate::formula::Scope { columns, pinned_at: &pinned_at })?;
            if c.ty != ColumnType::Boolean {
                return Err(Error::Invalid(format!("a filter is true or false, and `{f}` is {}", c.ty)));
            }
            format!(" WHERE {}", c.sql)
        }
        None => String::new(),
    };
    let mut order = Vec::new();
    for k in view.sort {
        let c = columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(&k.column))
            .ok_or_else(|| Error::Invalid(format!("there is no column '{}' to sort by", k.column)))?;
        order.push(format!("{} {} NULLS LAST", ident(&c.name), if k.desc { "DESC" } else { "ASC" }));
    }
    let order = if order.is_empty() { String::new() } else { format!(" ORDER BY {}", order.join(", ")) };
    let total = if filter.is_empty() { rows.num_rows() as u64 } else { session.count(&format!("SELECT count(*) FROM v{filter}"))? };
    let page = session.query(&format!("SELECT * FROM v{filter}{order} LIMIT {limit} OFFSET {offset}"))?;
    Ok((total, page))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::engine;

    #[test]
    fn a_page_read_from_the_file_is_the_page_the_engine_gives() {
        let e = engine();
        let s = e.session().unwrap();
        let columns: Vec<Column> = [
            ("b", ColumnType::Boolean),
            ("i", ColumnType::Integer),
            ("d", ColumnType::Decimal(2)),
            ("f", ColumnType::Double),
            ("day", ColumnType::Date),
            ("at", ColumnType::Timestamp),
            ("t", ColumnType::Text),
        ]
        .into_iter()
        .map(|(n, ty)| Column { name: n.into(), ty, role: crate::schema::Role::inferred(n, ty) })
        .collect();
        let exprs: Vec<String> = [
            "range % 2 = 0",
            "range * 7 - 3000",
            "range / 7.0",
            "CASE WHEN range % 5 = 0 THEN -0.0 ELSE range / 3.0 END",
            "DATE '2026-01-01' + (range % 400)::INT",
            "TIMESTAMP '2026-01-01 10:00:00' + to_seconds(range * 13)",
            "'v' || range",
        ]
        .iter()
        .zip(&columns)
        .map(|(x, c)| format!("CAST(CASE WHEN range % 11 = 3 THEN NULL ELSE {x} END AS {}) AS {}", c.ty.sql(), ident(&c.name)))
        .collect();
        let rows = s.query(&format!("SELECT {} FROM range(5000)", exprs.join(", "))).unwrap();
        assert!(rows.batches.len() > 1, "the rows span batches");
        let held: Vec<_> = rows.schema.fields().iter().map(|f| f.data_type().clone()).collect();
        assert_eq!(held, columns.iter().map(|c| c.ty.arrow()).collect::<Vec<_>>(), "the file is read straight");
        let dir = std::env::temp_dir().join(format!("zsql-prep-page-{}-{}", std::process::id(), crate::time::nanos()));
        let path = dir.join("rows.arrow");
        crate::store::write_rows(&path, &rows).unwrap();
        let json = |b: &Batches| zsql_exec::json::rows(b).unwrap();
        for (offset, limit) in [(0, 100), (0, 0), (2040, 20), (2048, 3000), (4990, 100), (6000, 10), (0, 10_000)] {
            let (n, fast) = page_of(&e, &columns, (&path, 5000), offset, limit, &View::default()).unwrap();
            let (m, slow) = page(&e, &columns, &crate::store::read_rows(&path).unwrap(), offset, limit, &View::default()).unwrap();
            assert_eq!((n, json(&fast)), (m, json(&slow)), "offset {offset}, limit {limit}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

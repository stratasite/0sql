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

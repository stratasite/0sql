# Parked enhancements

Expressiveness ideas examined on 2026-09-30 and parked. Each has a sketch
of the model or spec form, the SQL shape, and the reason it was parked, so
it can be picked up without redoing the analysis.

## Period-to-date (YTD, QTD, MTD, WTD)

Form: `temporalize` transforms `year_to_date` etc., alias `YTD(x)`.
Shape: without a date or measure filter, a running sum over the query's own
grain partitioned by the period's year (and the other projected dimensions);
with a filter, its own node with the date filter widened to the period
start, the running sum computed inside that node, and the widened-in periods
dropped at the final pass with a bound truncated to the grain.

Parked because the common case is already expressible: filter to this year
and every measure is year-to-date; `running_sum(x)` gives the cumulative
line; `yoy(x)` and `running_sum(yoy(x))` give prior year and prior year to
date. What only YTD can do is a filter window not aligned to the period
start, and a running sum that restarts each year when the result spans
several years. A cheaper fix for the second: let `running_sum` partition by
the year of the date projection.

## Entity-derived dimensions (inclusion rules for dimensions)

Form: an `inclusions` block on a dimension, `keys` (the entity grain),
`from` (the fact whose rows the aggregate runs over), `filter: apply | only
| ignore` (required), `sql` (an aggregating formula over `[Name]@d`/`@m`
references); `expression.table` instead of `expression.sql`. Same machinery
as the measure inclusion rule: an aggregation node at the key grain, joined
back on the keys, projected as a dimension. A spec-level variant is a
calculation with `keys`.

Parked because the attributes people reach for first (first purchase month,
lifetime tier, new versus returning) are static per entity and belong in
ETL. The runtime-only cases are attributes defined relative to the question:
rank or decile within the filtered population, top category within a chosen
set, share of a chosen whole, viewer-relative flags under row-level
security, position within a segment. Real, but narrow, and mostly served by
the spec-level form. If revived, the model form should refuse `filter:
ignore` or warn that the attribute is static.

## Fan-out-safe measures on one-side tables

Form: no model change. A measure whose table is the one side of a
many-to-one join (store square footage, headcount, credit limit, population,
capacity), asked at a grain only the fact reaches, is aggregated over
distinct entities per group: a `SELECT DISTINCT` node of the group
dimensions, the entity key (the right-side columns of the join condition,
or a declared primary key) and the measure's columns, with the measure's
expression run over it, merged on the shared dimensions. Denominators for
ratios (sales per square foot of the stores that traded, cases per capita of
the regions with cases) are the use.

Parked because an exclusion rule on the fact's dimensions already gives the
whole-set denominator (footage of all stores cross-joined onto every month),
and grouping by the entity's own attributes already merges correctly. The
gap is the participating-set denominator and fact-side filters on it. Worth
doing first: when a one-side measure is asked at a fact grain, the error
should say that an exclusion rule on the fact's dimensions gives the
whole-set denominator, instead of "could not find universe".

## Others listed, not examined

Mergeable distinct counts (declared distinct measures, sketches on
warehouses that have them); expressions spanning a joined table (row-grain
joins inside a measure); segment algebra (union, difference, nesting); top N
within a group (a partition on the top-n filter); hierarchies including
parent-child (recursive CTE flattening); dense time series (a period spine
so empty periods appear and windows are right); relative filters (measure
against measure, against an aggregate, latest N periods with data); plans
spanning datasources (one statement per datasource plus a merge
specification the caller executes).

## Field hit counts for ranking (parked)

Count a field once per query that plans successfully through sql or
explain (projections, filters, calculation references); explore, search
and tests do not count. Key by project and field uid, keep a score and a
last-seen time with exponential decay (half-life about thirty days), hold
it in memory on the server and persist a small JSON file per project in
the data directory, loaded at startup.

Use: fields search orders containing matches by hits; explore orders each
list by hits (match score first when a term is given); fuzzy resolution
uses hits only to break ties inside the margin, when one candidate's
share clearly dominates (about 3x), so popularity never overrides a better
string match and the feedback loop stays open. Lowering takes an optional
ranker (uid to score); without one behaviour is unchanged. A `hits` field
on fields and explore entries, and a top-fields endpoint per project that
doubles as a usage report.

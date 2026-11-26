# Filters that compare two fields

A filter compares its field with a value: `Ship Date > 2024-01-01`.
This note is the other kind, a field compared with a field:
`Ship Date > Order Date`, `Sales Price > Wholesale Cost`,
`Sales Price > Item Current Price`, `Bill City contains Ship City`.

## The spec

A leaf names the other field where it would give a value:

```json
{"field": "Catalog Ship Date", "predicate": "greater_than", "value_field": "Date"}
{"field": "Date", "predicate": "between", "value_field": "Start Date", "value_end_field": "End Date"}
```

`value_field` and `value_end_field` take a uid or a name with an
optional `@d`/`@m`, as `field` does. A leaf gives a value or a field,
never both, and `between` gives both bounds as fields or neither. The
shorthand writes it `ship date > [order date]`: a bracketed name in a
value's place. A bare name there is still a value, and a double-quoted
one is still the error it was (`country = "Canada"` is a value written
the SQL way far more often than a field meant).

## What compares with what

Both sides share a family. A measure without a type is a number, as a
calculation is.

| Family | Types | Predicates against a field |
|---|---|---|
| number | integer, bigint, decimal | `= != > >= < <=`, `between` |
| date | date, datetime | `= != > >= < <=`, `between` |
| string | string | `= !=`, contains, starts with, ends with and their `not` forms |
| boolean | boolean | `= !=` |

Binary fields, fields of different families and a field compared with
itself are refused when the spec lowers. `<` and `>` between strings
are refused too: legal SQL, but lowercase collation order is almost
never the question.

Strings compare as string values do, case-insensitively: both sides
are lowered. Contains and its kin are a search, not a pattern, so a
`%` or `_` in the data is never a wildcard. Each dialect spells the
search in `string_contains`, `string_starts_with` and
`string_ends_with` (STRPOS, CHARINDEX, INSTR, POSITION, or SUBSTR where
there is no REVERSE). On SQL Server, `CHARINDEX('', x)` is 0, so an
empty string on the value side contains nothing there and everything
elsewhere.

A date is compared with a datetime as the warehouse compares them;
nothing is cast. Most compare them implicitly. BigQuery refuses
DATE against DATETIME or TIMESTAMP, so that pair needs the model to
give both one type.

## Where it is applied

The filter's own field says where it goes. Lowering puts a measure
there: a dimension compared with a measure changes places and the
operator turns (`Current Price < Sales Price` becomes
`Sales Price > Current Price`), so `is_measure` on the filter is
"either side is a measure" and every HAVING/WHERE decision downstream
holds unchanged.

**Two dimensions** compare row by row, a WHERE in every node that
aggregates. Universe selection already makes every chosen universe
reach every dimension of the query, both sides of a comparison
included, so a pair no universe reaches together is refused by the
resolver as any unreachable dimension is. A compound measure's
sub-plan levels the query to its universe and drops what it cannot
reach; a comparison it reaches one side of is refused rather than
dropped, since dropping it answers another question. Measure rules
treat a comparison as a filter on both fields: a rule that ignores a
table's filters ignores a comparison touching that table, and one that
keeps only a table's filters keeps a comparison only when both sides
are on it.

**A measure** compares at the grain the query answers at. With one
aggregation, that is its HAVING. When the root is a merge (measures
from different universes, a rule's own node, a share's denominator, a
prior period) nothing below it applies the comparison, whatever the
dialect's `cross_universe_measure_filtering_strategy` says for value
filters: a child may hold one side only, or aggregate a measure at
another grain. `visitors::comparison` takes it out of every node below
the root, puts it on the root, and makes each measure it reads a
column of an input that aggregates it at the query's grain. A merge
that groups wraps each measure side in `max()` as it does any measure
filter. A group present on one side of a full outer join has NULL for
the other measure, and the comparison drops it, as a value filter on
that measure would.

**A measure against a dimension** needs the dimension in the grain:
projected, undecorated. Otherwise the comparison has no single
dimension value per group. The other reading, a row-level comparison
of the measure's inputs, is a calculated dimension's question, and the
refusal says so.

**Segments** may compare two dimensions in their filters. A segment's
measures define membership in a plan of their own at the keys' grain,
and comparing them there is not built; such a filter is refused.

## Not here

- `in` and `not in` against an array dimension's column.
- Mixed `between`: one bound a field and the other a value.
- Completing a field name inside `[` in the repl, which the left side
  of a filter does not do either.

# 0sql

Semantic layer to SQL, as a service. A project describes its warehouse
tables, dimensions, measures, joins and row-level security policies; 0sql
takes a query spec and a security context and returns the SQL, in
microseconds. Nothing about the caller's queries is stored.

```
crates/semantic   snapshot types, universe index and integrity checks
crates/formation  join routes per root table, computed from the join graph
crates/dialect    per-warehouse settings and templates (dwh gem YAML, compiled in)
crates/spec       the query spec, its lowering into the typed query, the formula parser, the security context
crates/planner    resolution, visitors, SQL generation
crates/project    a project directory (YAML models) loaded into a snapshot
crates/golden     golden cases and the scoreboard
crates/cli        0sql: check, plan, repl, test, export, golden, bench, form
dialects/         the dwh adapter settings the binary embeds (MIT)
fixtures/golden   golden cases and the universes they plan against
harness/          import of goldens from a roundhouse/strata-rs capture
```

## The contract

Input is a spec: projections (fields by uid or name, decorated), filters
(flat or an `and`/`or` tree), calculations (formulas over `[Name]@m`,
`[Name]@d` and `[Alias]` references) and segments (populations keyed on
dimensions, applied to the query or to named measures). Output is one SQL
statement for the project's warehouse, or an error naming what could not
be resolved. Filters are plain values: whoever lets a viewer change one
resolves it before sending the spec. There are no views, reports, linked
or interactive filters here; those belong to the caller.

The security context travels with the request: an email, admin flags, the
user's tags and groups with tags. Policies in the project decide from it
which rows and values the user may see.

```
0sql check  --project ../myproject
0sql plan   --project ../myproject --expr "item category, month(date), store net paid > 100"
0sql plan   --project ../myproject --spec q.json --context user.json
0sql repl   --project ../myproject
0sql test   --project ../myproject
0sql export --project ../myproject --out model.json.gz
0sql golden                       # planner parity with the goldens
0sql bench                        # phase timings (ZSQL_PROFILE=1 for detail)
```

The repl takes one comma-separated line and reads each item from its shape:
a predicate makes it a filter, a formula with a bracket reference is a
calculation (`Alias = formula` or `formula as Alias`), anything else is a
projection with decorator functions such as `month(date)`, `yoy(revenue)`,
`pct(revenue)`, `moving_avg(revenue, 7)`. `[Name]` or `"Name"` is always a
field. Commands start with a dot: `.fields`, `.tables`, `.json {...}`,
`.tojson <line>`, `.help`, `.quit`.

## Parity

The planner is a port of strata's Rails planner, proven against goldens
captured from that planner's own test suite (see `harness/`): every case
that a spec can express, on the main branch and on a mutated clone. All
385 pass, over paths formed here and over the paths Rails stored.

```
cargo test                     # contract, scoreboard, formation, spec round trips
ZSQL_GOLDEN_STRICT=1 cargo test -p zsql-golden
```

Where it improves on Rails, each covered by a test: a calculation that
references another calculation is inlined before a temporal transform; a
measure filter beside an inclusion measure stays on the node that computes
that measure; equal-cost join routes and merge-input order follow names
rather than row ids, so the same model and query produce the same SQL on
every deploy.

## Planning cost

A `Planner` is built once per loaded model and reused for every request;
the index build is the only per-model cost. Planning a golden case takes
about 70 microseconds in release builds, about a third of it in resolution
and the rest in SQL generation.

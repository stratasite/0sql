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
crates/cli        zsql: check, plan, repl, test, export, golden, bench, form
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
which rows and values the user may see. A policy fires when the query
projects a field it names by tag or by name; it then resolves the values
of its context dimension the user is allowed, from the groups' names or
tags or from the user's email or tags, and either filters rows to those
values or masks the field outside them. A fired policy that resolves
nothing denies by default (no rows, or every value masked); a policy may
say `unresolved: allow` under `permission_resolution` to be skipped
instead, which is what strata does. Bypass flags open the door explicitly.

```
zsql check  --project ../myproject
zsql plan   --project ../myproject --expr "item category, month(date), store net paid > 100"
zsql plan   --project ../myproject --spec q.json --context user.json
zsql repl   --project ../myproject
zsql test   --project ../myproject
zsql export --project ../myproject --out model.json.gz
zsql values sample --project ../myproject          # dimension values into values.jsonl.gz
zsql values show   --project ../myproject category
zsql golden                       # planner parity with the goldens
zsql bench                        # phase timings (ZSQL_PROFILE=1 for detail)
```

The repl takes one comma-separated line and reads each item from its shape:
a predicate makes it a filter, a formula with a bracket reference is a
calculation (`Alias = formula` or `formula as Alias`), anything else is a
projection with decorator functions such as `month(date)`, `yoy(revenue)`,
`pct(revenue)`, `moving_avg(revenue, 7)`. `[Name]` or `"Name"` is always a
field. Commands start with a dot: `.fields`, `.tables`, `.json {...}`,
`.tojson <line>`, `.help`, `.quit`.

## The server

`zsqld` is the light service: deploy a project, get SQL back. No accounts,
users or keys yet. Deployed models live in memory and under `ZSQL_DATA_DIR`
(default `data/`), so a restart reloads them. `ZSQL_BIND` sets the address
(default `127.0.0.1:8080`).

```
cargo run --release -p zsql-server
zsql deploy --project ../myproject --server http://localhost:8080
curl -s -X POST localhost:8080/projects/myproject/branches/main/plan \
     -H 'content-type: application/json' \
     -d '{"spec": {"projections": [{"field": "department"}, {"field": "sales value"}]}, "context": {...}}'
```

| route | what |
|---|---|
| `POST /projects/{uid}/branches/{branch}/deploy` | body: tar.gz of the project; `zsql deploy` builds it without secrets |
| `POST /projects/{uid}/branches/{branch}/plan` | `{spec, context?, explain?}` to `{sql, adapter, timings_us}`, or `{error: {class, message}}` |
| `GET /projects/{uid}/branches/{branch}` | summary and warnings |
| `GET /projects/{uid}/branches/{branch}/fields?q=&hidden=` | field discovery (hidden fields excluded by default) |
| `GET /projects/{uid}/branches/{branch}/tables` | tables with their fields |
| `GET /projects/{uid}/branches/{branch}/values/{dim}` | sampled values of a dimension |
| `GET /projects`, `GET /healthz` | every deployment; liveness |

A branch with security policies refuses to plan without a context.

## Dimension values

`zsql values sample` reads each listable dimension from the cheapest table
that holds it and writes `values.jsonl.gz` in the project: for text, the
top `value_list_size` values by row count (default 1000) with their counts,
or every value with `values: full` on the field (refused above 100k
distinct); for numbers and dates, the min and max. Hidden dimensions,
listing-disabled ones, `values: none`, and any dimension a security policy
names are never sampled. The file is customer data: gitignore it unless
you want it reviewed. `--dims a,b` limits the run, `--older-than 7`
resamples only stale entries, `--dry-run` prints the SQL. Connections come
from `datasources.yml`, secrets from `.strata` in the project (the strata
CLI's credential file) or `ZSQL_DS_<KEY>_PASSWORD`; duckdb and postgres
are the drivers so far.

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

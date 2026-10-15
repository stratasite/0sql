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
crates/cli        zsql: init, deploy, and every server endpoint; no engine inside
crates/devtool    zsql-dev: the engine on the command line (local plan, goldens, bench)
crates/shorthand  the comma-separated query shorthand, shared by the CLI and the server
crates/service    a loaded model and every read over it (sql, explain, explore, fields, tests), shared by zsql and zsqld
crates/accounts   accounts, users, keys and project access (SQLite)
crates/server     zsqld: deploy a project, plan specs
crates/warehouse  duckdb and postgres drivers for value sampling
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
A measure's own exclusion or inclusion rule on a policy's context
dimension outranks the policy, as it outranks a caller's filter: a
`total across all regions` measure is the unsecured total for everyone,
and so is the list of regions beside it. That is the modeler's decision,
made in the model where only the deployer can make it; a denial still
holds. Who a query is planned for can also be fixed on a query key by
the project's owner, see `docs/mcp.md`.

## The CLI

`zsql` authors a project and talks to a server. It generates no SQL
itself: planning, validation and tests all run on zsqld, so the engine
never leaves the service. The CLI keeps the warehouse credentials, which
is why value sampling runs there.

```
zsql init [DIR] [--name N]        # project.yml, datasources.yml, security.yml, models/, tests/, samples/, git init
zsql new table Orders --datasource warehouse [--domain sales]
zsql new relation --datasource warehouse
zsql new test smoke
zsql auth --api-key KEY [--server URL]   # written to .zsql (gitignored); .strata is read when .zsql is absent
zsql check                        # validate on the server, keep nothing
zsql deploy [--watch] [--dry-run] # the checked-out git branch, as the server's branch
zsql test                         # the deployed branch's tests/*.yml
zsql sql --expr "department, sales value, department = PRODUCE" [--context user.json] [--explain] [--json]
zsql explain --expr "..." | --spec query.json   # the SQL, the node graph and timings
zsql explore --expr "department, sales value" [-q customer]   # the dimensions and measures the query can take on
zsql spec department, sales value, department = PRODUCE   # the spec JSON a line makes, locally
zsql repl [--context user.json]   # the shorthand, planned on the server; .explain, .branch
zsql fields depart | zsql tables | zsql values show department | zsql status | zsql list | zsql health
zsql values sample [--dims a,b] [--full] [--older-than 7] [--dry-run] [--deploy]   # runs the SQL locally: needs the full build
zsql remove --yes                 # drop the deployed branch
```

The default build has no warehouse drivers: it talks to the server only,
builds in seconds on every platform, and is what the installer ships.
`cargo build -p zsql-cli --features warehouse` adds DuckDB and Postgres
for `zsql values sample`.

Every command takes `--project DIR`, `--branch NAME`, `--server URL` and
`--uid UID`; without them the project is found from `project.yml` here or
above, the branch is the checked-out git branch, and the server comes from
`ZSQL_SERVER`, `.zsql`, `project.yml`'s `server:` (for a self-hosted
zsqld) or the local default. `project.yml`'s `production_branch` names the
branch production reads; deploying to it is called out.

Deploys carry the YAML models, tests, `security.yml`, `datasources.yml`
with its secrets removed, and `samples/values.jsonl.gz`. The server
answers with counts, warnings and the test results.

The repl takes one comma-separated line and reads each item from its shape:
a predicate makes it a filter, a formula with a bracket reference is a
calculation (`Alias = formula` or `formula as Alias`), anything else is a
projection with decorator functions such as `month(date)`, `yoy(revenue)`,
`pct(revenue)`, `moving_avg(revenue, 7)`. `[Name]` or `"Name"` is always a
field. Commands start with a dot: `.fields`, `.tables`, `.values`,
`.explore <line> [? term]`, `.json {...}`, `.spec <line>`, `.branch`, `.help`, `.quit`. Inside a project,
`zsql` with no command opens the repl.

`zsql-dev` (the devtool crate) is the engine on the command line for its
own developers: local planning over a snapshot or project directory, the
golden scoreboard, the bench, formation checks and export. It is not part
of the CLI release.

## The server

`zsqld` is the light service: deploy a project, get SQL back. No accounts,
users or keys yet. Deployed models live in memory and under `ZSQL_DATA_DIR`
(default `data/`), so a restart reloads them. `ZSQL_BIND` sets the address
(default `127.0.0.1:3699`).

```
cargo run --release -p zsql-server
zsql deploy --project ../myproject --server http://127.0.0.1:3699
curl -s -X POST 127.0.0.1:3699/projects/myproject/branches/main/sql \
     -H 'content-type: application/json' \
     -d '{"spec": {"projections": [{"field": "department"}, {"field": "sales value"}]}, "context": {...}}'
```

| route | what |
|---|---|
| `POST /projects/{uid}/branches/{branch}/deploy` | body: tar.gz of the project; `zsql deploy` builds it without secrets; tests run, results in the answer |
| `POST /projects/{uid}/branches/{branch}/validate` | the same, keeping nothing |
| `POST /projects/{uid}/branches/{branch}/test` | run the deployed tests again |
| `GET /projects/{uid}/branches/{branch}/sampling-plan` | the SQL the CLI runs to sample values |
| `DELETE /projects/{uid}/branches/{branch}` | drop a deployment |
| `POST /projects/{uid}/branches/{branch}/sql` | `{spec, context?}` or `{expr, context?}` (a shorthand line; the answer then carries the `spec` it made and `items`) to `{sql, datasource, datasource_uid, adapter, corrections?}`, or `{error: {class, message}}` |
| `POST /projects/{uid}/branches/{branch}/explain` | the same request, answered with `{sql, datasource, datasource_uid, adapter, timings: {parser_us, plan_us, total_us}, phases: [{name, us}], nodes: [...]}`: each node's kind, CTE alias, table, join paths, projections, filters, inputs, strategies and identity |
| `POST /projects/{uid}/branches/{branch}/explore` | `{spec, context?, q?}` to the visible dimensions and measures the query can add and still resolve, from a walk of the formed universes: `{dimensions: [{uid, name, data_type, tables, score?}], measures: [...], total_us}`; `q` keeps fields containing or near the term, best first |
| `GET /projects/{uid}/branches/{branch}` | summary and warnings |
| `GET /projects/{uid}/branches/{branch}/fields?q=&hidden=` | field discovery: containing matches, then near matches with a `score` (hidden fields excluded by default) |
| `GET /projects/{uid}/branches/{branch}/tables` | tables with their fields |
| `GET /projects/{uid}/branches/{branch}/values/{dim}` | sampled values of a dimension |
| `GET /projects`, `GET /healthz` | every deployment; liveness |
| `POST /mcp` | the reads above as MCP tools for an agent, under the same key; `docs/mcp.md` |

A branch with security policies refuses to plan without a context.

With `ZSQL_ACCOUNTS=on` the server is multi-tenant: accounts, users,
personal keys and read-only query keys with per-project grants, in a
SQLite file in the data directory; see `docs/accounts-and-access.md` for
the model and the `/signup`, `/me`, `/account/...` and `/projects/{uid}`
endpoints, and the web app at `/app/` for sign-up, keys, members and
project access. Off, it is the single-tenant self-hosted mode with no auth.

An account may keep 5 deployed branches per project and 20 in all
(`ZSQL_MAX_BRANCHES_PER_PROJECT`, `ZSQL_MAX_BRANCHES_PER_ACCOUNT`, `off`
for neither), since a deployed model stays in memory for the life of the
process. Going over answers `409 OverLimit` and names what to remove.

Password reset and email verification are emailed links. The server has
no SMTP client: it writes the message to `ZSQL_MAIL_COMMAND` on stdin
(`msmtp -t`, `sendmail -t`, a script), with `ZSQL_MAIL_FROM` as the
sender and `ZSQL_PUBLIC_URL` as the base of the link. With no command
configured the links go to the journal instead, never into a response,
so a self-hosted server works without mail.

`docs/running-it.md` is where the service actually runs, how a release
reaches it, how the backups work and what is still open.

Requests are rate limited and admitted by cost class: planning, exploring,
deploying, account changes and the password routes each have their own
per-account allowance and their own cap on how many may run at once, and
deploys and password hashing run off the request threads. Bodies and
query specs are bounded too. With accounts off only the admission caps
and those bounds apply. `docs/rate-limiting.md` has the design, the
settings and the measurements; `429` and `503` both say when to come
back, and the CLI waits and asks again.

Each deployment keeps its planner, the per-root universe index, built
once at deploy beside the universe; requests borrow it, so server time
is lowering plus planning only.

`[Name]` inside a formula names a field in a model expression, and a
projection of the same query (then a field) in a calculation; `@d` and
`@m` pick the dimension or the measure, and in a calculation they also
say you mean the field rather than a projection that shares the name. A
name belongs to at most one dimension and at most one measure, so a bare
reference is ambiguous only when both exist, which is when the suffix is
required. Square brackets always name a field: a warehouse column goes
in bare or in double quotes, whatever its warehouse uses for quoting.

A field reference resolves by uid, name or synonym, case ignored. One that
matches nothing resolves to the nearest field by character trigram
similarity when that field clearly leads (score 0.5 or more, 0.2 ahead of
the next); the answer lists it under `corrections` and the CLI echoes
`departmnt → Department (~0.8)`. Anything less clear fails with the closest
fields named. References under four characters only resolve exactly.

The CLI covers every endpoint; see above.

## Dimension values

`zsql values sample` asks the server which dimensions to sample and the SQL
to run (the server knows the model), runs it against the project's
warehouses with the local credentials, and writes `samples/values.jsonl.gz`:
for text, the
top `value_list_size` values by row count (default 1000) with their counts,
or every value with `values: full` on the field (refused above 100k
distinct); for numbers and dates, the min and max. Hidden dimensions,
listing-disabled ones, `values: none`, and any dimension a security policy
names are never sampled. The file is customer data: gitignore it unless
you want it reviewed. `--dims a,b` limits the run, `--older-than 7`
resamples only stale entries, `--dry-run` prints the SQL, `--deploy`
redeploys afterwards. Connections come from `datasources.yml`, secrets from
`.zsql` (or `.strata`) in the project or `ZSQL_DS_<KEY>_PASSWORD`; duckdb
and postgres are the drivers so far.

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

End to end over HTTP, on a twelve-thread laptop against the TPC-DS sample,
the `sql` endpoint answers about 42,000 requests a second with accounts on
and about 39,000 with the rate limits enforcing. Authentication is cached
in memory for a minute and emptied by every change, which is what moved
that from 13,000; see `docs/rate-limiting.md`.

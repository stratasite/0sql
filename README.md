# 0sql

**Don't let the model write SQL.** Your agent or your application sends
a query spec in business terms; 0sql resolves it against your governed
semantic model into one correct, deterministic SQL statement, or a
refusal that says why. It hands the statement back for you to run, or
runs it on your warehouse itself and returns the rows, keeping the
result to answer the same question again.

One binary, `zsql`, is the command line, the planner and the
server. Open source under MIT. Docs at [0sql.io/docs](https://0sql.io/docs/),
a live playground at [0sql.io/try](https://0sql.io/try/).

## Getting started

Install it:

```sh
curl -fsSL https://0sql.io/install.sh | sh
```

It runs on macOS 11 or newer and on Linux with glibc 2.34 or newer
(Ubuntu 22.04, Debian 12, RHEL 9, Amazon Linux 2023). Not on Alpine:
base a container on one of those.

Start a project and describe one table:

```sh
zsql init shop
cd shop
```

```yaml
# datasources.yml
warehouse:
  adapter: postgres
  host: db.internal
  database: shop
  username: reader
```

```yaml
# models/sales/tbl.orders.yml
name: Orders
physical_name: orders
datasource: warehouse
cost: 100
fields:
  - type: dimension
    name: Order Date
    data_type: date
    expression: ordered_at
  - type: dimension
    name: Country
    data_type: string
    expression: ship_country
  - type: measure
    name: Revenue
    data_type: decimal
    expression: sum(amount)
  - type: measure
    name: Orders
    data_type: integer
    expression: count(*)
```

Ask for SQL, from the files at hand:

```
$ zsql sql --expr "country, revenue, orders, country = Canada"
SELECT
	T0."ship_country" AS "Country",
	sum(T0."amount") AS "Revenue",
	count(*) AS "Orders"
FROM
	orders T0
WHERE
	LOWER(T0."ship_country") = 'canada'
GROUP BY
	T0."ship_country"
-- planned here in 1.8 ms
```

Serve it, and let your application ask:

```sh
zsql serve
```

```sh
curl -s http://127.0.0.1:3699/projects/shop/branches/main/sql \
  -H 'content-type: application/json' \
  -d '{"spec": {"projections": [
        {"field": "Order Date", "decorators": [{"type": "truncate", "grain": "month"}]},
        {"field": "Revenue"}]}}'
```

```json
{"sql": "SELECT\n\tDATE_TRUNC('month', T0.\"ordered_at\")::DATE AS \"Month(Order Date)\",\n\tsum(T0.\"amount\") AS \"Revenue\"\nFROM\n\torders T0\nGROUP BY\n\tDATE_TRUNC('month', T0.\"ordered_at\")::DATE",
 "datasource": "warehouse", "datasource_uid": "warehouse", "adapter": "postgres"}
```

Run that statement against your own warehouse, with your own
credentials, in your own code. Or have the server run it: put the
datasource's password in `.zsql`, which stays out of git, and ask for
rows instead. A deploy carries it, and `zsql serve` deploys the
project it is started in.

```yaml
# .zsql
warehouse:
  password: ...
```

```sh
curl -s http://127.0.0.1:3699/projects/shop/branches/main/run \
  -H 'content-type: application/json' \
  -d '{"expr": "country, revenue, orders"}'
```

```json
{"id": "r-20261124-3712070077e83074d38e", "type": "semantic", "status": "succeeded",
 "cache": "miss", "row_count": 2, "complete": true,
 "columns": [{"name": "Country", "type": "string", "arrow_type": "Utf8"},
             {"name": "Revenue", "type": "string", "arrow_type": "Utf8"},
             {"name": "Orders", "type": "string", "arrow_type": "Utf8"}],
 "rows": [["Canada", "18250.00", "112"], ["Mexico", "9120.50", "64"]]}
```

Ask again and `cache` is `hit`: the stored result answers. The
[quickstart](https://0sql.io/docs/getting-started/quickstart/) covers
the model, joins and security in about ten minutes.

## What it does

- **Resolves names to SQL.** A request names fields, not columns. The
  planner finds the tables that hold them, the join route between them
  and the aggregation each measure needs.
- **Blends across fact tables safely.** Measures from different facts at
  different grains are aggregated apart and stitched on the conformed
  dimensions. No fan-out, no double counting.
- **Applies your security policies.** Each request carries a security
  context; policies in the model become `WHERE` filters or `CASE` masks
  in the statement. A branch with policies refuses to plan without one.
- **Speaks 13 dialects.** One model, one request shape, SQL for
  Postgres, Snowflake, BigQuery, Databricks, Redshift, Athena, Trino,
  ClickHouse, Druid, DuckDB, MySQL, SQL Server and SQLite.
- **Refuses before it is wrong.** A request that cannot be resolved
  comes back as an error naming what was not found and the nearest
  fields; a near miss is corrected and reported.
- **Runs it, when you want it to.** The server runs the statement it
  planned on the datasource the plan chose and returns typed rows, as
  JSON or Arrow, now or by an id to ask again with. Or it only plans,
  and you run the statement yourself: nothing connects until a run is
  asked for.
- **Remembers the answer.** A result answers the same question again
  until the datasource's `cache_ttl` passes, or until you say a table
  it read has changed.
- **Works with agents.** `POST /mcp` offers the same reads as MCP
  tools, so an agent plans against the model instead of guessing at the
  schema, and can be given tools to run what it plans. The shorthand (`country, revenue, country =
  Canada`) is for people at a prompt; agents write specs.

The model is YAML in git: branches, pull requests, tests (`zsql check`),
and a deploy that is the checked-out branch.

## How the server works

The server is a component, used the way a database is. It sits behind
your own service, which signs your users in, decides which projects
each may ask about, sends the security context each plan runs under,
and paces its callers. It has no accounts, keys or token of its own
and answers whoever can reach it, a deploy included. Bound to the
loopback it is reached only from its own machine; a bind beyond that
has to be meant, with `--open`.

**Deploy.** `zsql deploy` sends the checked-out branch as an archive.
The server compiles it, runs its tests, and holds the compiled model in
memory and on disk, one per org, project and branch. A restart reloads them.

**Plan.** `POST .../sql` takes a spec or an expression and a security
context and returns the statement, the datasource and the dialect.
`explain` adds how it was planned, `explore` says what the query can
still take on, and `fields`, `tables` and `values` read the catalog.

**Run.** `POST .../run` plans the same request and runs it.

1. The run is recorded before anything else, as `queued`.
2. A stored result that still answers is returned. An identical run
   already in flight is joined rather than started twice.
3. Otherwise it takes one of `ZSQL_CONCURRENT_RUNS` seats, or waits in
   a queue with an end.
4. The statement runs on the datasource, under its `query_timeout`,
   with the credential the request sent or the one the datasource was
   deployed with. Rows go to disk a batch at a time, so a
   large result costs disk and time, not memory.
5. The request waits `query_wait_time` (ten seconds unless the
   datasource says). Ready in time, the rows are in the answer.
   Otherwise the answer is the id, and `GET /results/{id}` returns the
   rows a page at a time, or whole as Arrow; `DELETE` cancels.

`POST .../sql-exec` runs one statement of the caller's own, as
written. It is never cached and it may write: what a credential may do
is the database's to enforce, not the server's.

**The cache.** A result is found by the project and the planned
statement, which already carries the row policies; not by the user or
the credential. `fresh: true` runs anyway, `cache_ttl: 0` on a
datasource keeps nothing, and `POST /cache/invalidate` with a
datasource and the tables that changed stops every result that read
them from answering.

**Orgs.** A project belongs to an org, and a server may keep the
projects of several. Every route above is also served under
`/orgs/{org}`; without the prefix the org is `default`. Nothing
crosses: a listing, a stored result, an invalidation and a warehouse
connection are one org's. An org is a directory of the data directory
and nothing more, and `zsql` reads it from `org:` in `project.yml` or
`--org`.

**Credentials.** `zsql deploy` carries each datasource's secrets from
`.zsql`, and on the server they are that datasource's credential for
the branch: kept apart from the compiled model, never served. A
request that sends a secret of its own connects with that and nothing
else. A later deploy that carries none keeps what the branch had, so a
pipeline can deploy without holding the secret.

**A record of each deploy.** With `ZSQL_OBJECT_STORE` set, every
deploy is also kept there as a zip: the project files, the compiled
model and what the deploy answered, with every secret left out.

**More than one node.** With `ZSQL_DB` the records of runs live in a
Postgres table every node shares, and with `ZSQL_OBJECT_STORE` the
results live in S3, GCS, Azure or a directory, with a size-limited copy
on each node. Without them a server keeps both on its own disk.

**Reaching a warehouse.** Postgres, Redshift, MySQL and SQL Server
over their wire protocols; Snowflake, BigQuery, Databricks, Trino,
ClickHouse and Athena over HTTP; DuckDB and SQLite in the process.
Snowflake and BigQuery go through their ADBC drivers when one is
installed (`dbc install snowflake`; `zsql datasource drivers` says what
is found). DuckDB and the drivers return each column as its type; the
others return text today.

Since the server returns rows, reaching its port is reaching your
warehouse. `docs/operating.md` says what that means for whoever runs
one, and `docs/execution.md` is the design.

## Where to go

| You want to | Read |
|---|---|
| Send your first request | [Quickstart](https://0sql.io/docs/getting-started/quickstart/) |
| Write the YAML model | [Semantic model](https://0sql.io/docs/semantic-model/) |
| Construct a request: projections, filters, calculations, segments | [Query API](https://0sql.io/docs/query/) |
| Lock rows down per user | [Row-level security](https://0sql.io/docs/security/) |
| Run the server | [Running the server](https://0sql.io/docs/getting-started/server/) and [`docs/operating.md`](docs/operating.md) |
| Understand how a run, the cache and a cluster work | [`docs/execution.md`](docs/execution.md) |
| Drive it from a terminal or CI | [zsql CLI](https://0sql.io/docs/cli/) |
| Put it behind an agent | [MCP](https://0sql.io/docs/mcp/) and the [chat dashboard tutorial](https://0sql.io/docs/examples/chat-dashboard/) |
| Look up an endpoint | [API reference](https://0sql.io/docs/api-reference/) |
| Point an agent at the docs | [llms.txt](https://0sql.io/docs/llms.txt) |

## The design, briefly

A **project** is a directory: `project.yml`, `datasources.yml`,
`security.yml`, tables and relations under `models/`, tests under
`tests/`. A **spec** is projections, filters, calculations and segments;
a **security context** is who the query is planned for. From those the
planner resolves a universe (a root table and the join routes it can
safely reach), builds a graph of nodes, runs its visitors and renders
one statement in the datasource's dialect. The same code plans on your
machine and on the server, held byte-identical by a test. Planning a
query takes tens of microseconds; a server that only plans answers
tens of thousands of requests a second on a laptop. Running one takes
as long as the warehouse does.

The planner is its own implementation, written with the
[Strata](https://strata.do) planner as its specification and held to
385 golden cases captured from it, on formed join paths and on stored
ones. 0sql is the open source core of that platform.

## Building from source

The toolchain `rust-toolchain.toml` names (Rust 1.98), and a C and C++
compiler for the SQLite and DuckDB the CLI bundles to run queries.

```sh
cargo build --release -p zsql-cli              # target/release/zsql
cargo build -p zsql-cli --no-default-features  # the CLI without the server
cargo test --workspace
cargo xtask golden --strict
```

```
crates/semantic   the model as the planner sees it: snapshot types, universe index, integrity checks
crates/formation  join routes per root table, formed from the join graph
crates/dialect    the 13 dialects: settings and templates, compiled in from dialects/
crates/spec       the query spec, its lowering, the formula parser, the security context
crates/planner    resolution, the node graph, the visitors, SQL
crates/project    a project directory loaded into a snapshot
crates/shorthand  the comma-separated line, for people at a prompt
crates/core       a loaded model and every read over it, shared by the CLI and the server
crates/datasource reaching a warehouse: the connection, its secret, a statement and its rows as they arrive
crates/exec       running statements: the record of a run, the queue, the result store, the cache, the object store
crates/log        where the log goes: level, format and destination, in one place
crates/server     the server: deploy, plan, run, MCP
crates/cli        zsql
xtask/            cargo xtask: the goldens, benches and formation checks, for developers of the engine
crates/golden     the golden cases and the scoreboard
fixtures/         public sample projects (TPC-DS, a grocery store) and the golden cases
harness/          how the golden cases were captured, and the MCP wording evaluation
docs/             design notes: execution, admission, MCP, operating and releasing
ops/              the systemd unit, and the table a server's records are kept in
```

`docs/operating.md` is for whoever runs a server for others: the data
directory, who may reach it, the settings, what changes shape on disk
and when.
`docs/releasing.md` is how a release is cut.

## License

MIT. See [LICENSE](LICENSE). The dialect settings under `dialects/`
carry their own MIT license file.

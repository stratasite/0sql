# 0sql

**Don't let the model write SQL.** Your agent or your application sends
a query spec in business terms; 0sql resolves it against your governed
semantic model and returns one correct, deterministic SQL statement, or
a refusal that says why. It never connects to your warehouse and never
runs a query.

One static binary, `zsql`, is the command line, the planner and the
server. Open source under MIT. Docs at [0sql.io/docs](https://0sql.io/docs/),
a live playground at [0sql.io/try](https://0sql.io/try/).

## Getting started

Install it:

```sh
curl -fsSL https://0sql.io/install.sh | sh
```

Start a project and describe one table:

```sh
zsql init shop
cd shop
```

```yaml
# datasources.yml
warehouse:
  adapter: postgres
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
credentials, in your own code. The [quickstart](https://0sql.io/docs/getting-started/quickstart/)
takes it from here, joins and security included, in about ten minutes.

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
- **Plans, never runs.** 0sql is given the shape of your warehouse,
  never its contents. No connection, no rows, no cache; a spec is
  planned in memory and never stored.
- **Works with agents.** `POST /mcp` offers the same reads as MCP tools
  under the same keys, so an agent plans against the model instead of
  guessing at the schema. The shorthand (`country, revenue, country =
  Canada`) is for people at a prompt; agents write specs.

The model is YAML in git: branches, pull requests, tests (`zsql check`),
and a deploy that is the checked-out branch. Accounts, users, personal
keys and read-only query keys with per-project grants are built in and
off by default; turn them on when the server is shared.

## Where to go

| You want to | Read |
|---|---|
| Send your first request | [Quickstart](https://0sql.io/docs/getting-started/quickstart/) |
| Write the YAML model | [Semantic model](https://0sql.io/docs/semantic-model/) |
| Construct a request: projections, filters, calculations, segments | [Query API](https://0sql.io/docs/query/) |
| Lock rows down per user | [Row-level security](https://0sql.io/docs/security/) |
| Run the server, with accounts and keys | [Running the server](https://0sql.io/docs/getting-started/server/) |
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
query takes tens of microseconds; the server answers tens of thousands
of requests a second on a laptop.

The planner is its own implementation, written with the
[Strata](https://strata.do) planner as its specification and held to
385 golden cases captured from it, on formed join paths and on stored
ones. 0sql is the open source core of that platform.

## Building from source

The toolchain `rust-toolchain.toml` names (Rust 1.98), and a C compiler
for the SQLite behind accounts.

```sh
cargo build --release -p zsql-cli              # target/release/zsql
cargo build -p zsql-cli --no-default-features  # the CLI without the server
cargo test --workspace
cargo run --release -p zsql-devtool -- golden --strict
```

```
crates/semantic   the model as the planner sees it: snapshot types, universe index, integrity checks
crates/formation  join routes per root table, formed from the join graph
crates/dialect    the 13 dialects: settings and templates, compiled in from dialects/
crates/spec       the query spec, its lowering, the formula parser, the security context
crates/planner    resolution, the node graph, the visitors, SQL
crates/project    a project directory loaded into a snapshot
crates/shorthand  the comma-separated line, for people at a prompt
crates/service    a loaded model and every read over it, shared by the CLI and the server
crates/accounts   accounts, users, keys and project access (SQLite)
crates/server     the server: deploy, plan, accounts, the web app, MCP
crates/cli        zsql
crates/devtool    zsql-dev: the goldens, benches and formation checks, for developers of the engine
crates/golden     the golden cases and the scoreboard
fixtures/         public sample projects (TPC-DS, a grocery store) and the golden cases
harness/          how the golden cases were captured, and the MCP wording evaluation
docs/             design notes: accounts and access, rate limiting, MCP, operating and releasing
```

`docs/operating.md` is for whoever runs a server for others: the data
directory, the settings, backups, what changes shape on disk and when.
`docs/releasing.md` is how a release is cut.

## License

MIT. See [LICENSE](LICENSE). The dialect settings under `dialects/`
carry their own MIT license file.

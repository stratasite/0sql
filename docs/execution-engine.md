# Execution engine: what exists, and what fits

An investigation, 2026-10-18. No decision is taken here; this is the
survey a decision would rest on. The engine's job: given the statement
the planner returns, in one of the thirteen dialects, plus a datasource
that today carries only a name, an adapter and a tier, connect to the
warehouse, run it, and hand the rows back. The README's promise that
0sql "never connects to your warehouse" would become a mode rather than
a property, which is a product decision as much as a technical one and
is left to the side here.

Three constraints were set for the ideal answer, and one more follows
from the product:

1. It does not bloat the binary.
2. It does not dramatically lengthen compile times.
3. It covers all thirteen adapters.
4. It can later carry OAuth to the warehouses that use it, and it
   performs well.

## What the binary is today

One static musl binary, `zsql`, for Linux x86_64 and arm64, and a
glibc-free build is the reason it runs on any distribution
(`docs/releasing.md`). Also macOS on both architectures.

| | |
|---|---|
| Release binary, unstripped, x86_64 | 18.2 MB |
| Crates in `Cargo.lock` | 231 |
| Already linked | tokio, hyper, axum, rustls, ureq (sync HTTP, CLI only), rusqlite (C, bundled) |
| Not linked | arrow, reqwest, any database driver but SQLite |

The static-musl choice is the hinge of this whole evaluation: anything
that reaches a warehouse by loading a prebuilt vendor shared library at
runtime (ADBC's Go and C++ drivers, ODBC, DuckDB's extensions, a
prebuilt libduckdb) cannot live inside that binary, because `dlopen` in
static musl is a stub. Drivers that are themselves Rust or C source can
be linked in (ADBC's driver manager has `load_static` for exactly this;
DuckDB core extensions can be built in), but those are the minority. So
every candidate below is either source compiled in, or lives in a
second process.

## The thirteen, grouped by how they are reached

The warehouses fall into three groups, and the grouping matters more
than any one library.

**Reached over HTTPS with a JSON API, bearer token in a header.** Eight
of the thirteen have a production-grade SQL-over-HTTP API that a plain
HTTP client can drive, and on every one of them an OAuth or IAM
credential is a header on the request, nothing more.

| Warehouse | API | Result format | Auth on the wire |
|---|---|---|---|
| Snowflake | SQL API v2, `POST /api/v2/statements`, partitions by `?partition=N` | JSON strings only | `Authorization: Bearer` with OAuth, key-pair JWT, PAT or WIF; password not accepted |
| Databricks | Statement Execution API 2.0, `POST /api/2.0/sql/statements`, `wait_timeout` 5–50 s then poll, cancel endpoint | JSON_ARRAY inline (25 MiB cap) or ARROW_STREAM / CSV via presigned external links | Bearer: PAT, OAuth U2M, OAuth M2M, Entra |
| BigQuery | REST v2 `jobs.query` / `getQueryResults` | JSON (10 GB compressed cap) | Bearer: user OAuth, service account, WIF |
| Trino | client protocol, `POST /v1/statement` then `nextUri`; spooled `json+zstd` since 466 | JSON; no Arrow encoding (issue #24586 open) | Bearer JWT, Basic; opaque tokens unsupported server-side |
| Druid | `POST /druid/v2/sql`, `resultFormat` array/arrayLines/csv; async `/druid/v2/sql/statements` | JSON | Basic, or Bearer JWT via pac4j |
| ClickHouse | HTTP interface | RowBinary, Native, JSONCompact, or `FORMAT ArrowStream` | user/password headers; Bearer JWT (Cloud) |
| Redshift | Data API, `ExecuteStatement` / `DescribeStatement` / `GetStatementResult`, 500 MB cap, 24 h retention | JSON cells (V2: CSV chunks) | SigV4; DB identity via Secrets Manager ARN, IAM temp user, or Identity Center |
| Athena | `StartQueryExecution` / `GetQueryExecution` / `GetQueryResults` (1000 rows a page, every cell a string); `UNLOAD` to Parquet for bulk | strings, or Parquet in S3 | SigV4 only, by design |

**Reached over a database wire protocol.** Four need a protocol
implementation, and each has a maintained pure-Rust one.

| Warehouse | Crate | Version | Async | Token auth today |
|---|---|---|---|---|
| Postgres (and Redshift over the wire) | `tokio-postgres` | 0.7.18, 2026-06 | tokio | cloud tokens as password (RDS IAM, Azure Entra, Cloud SQL) work; PG 18 SASL OAUTHBEARER not yet (PR #1378 open) |
| MySQL | `mysql_async` | 0.37.1, 2026-09 | tokio | cloud tokens via cleartext plugin over TLS; MySQL's OIDC plugin is Enterprise-only and unimplemented |
| SQL Server / Azure SQL | `tiberius` | 0.13.0, 2026-09, revived after a 2024–2026 lull | runtime-agnostic | `AuthMethod::aad_token` sends an Entra access token over TDS FEDAUTH |
| SQLite | `rusqlite` | already linked | sync | n/a |

**In-process engines.** DuckDB is a library, not a server. The
`duckdb` crate is Arrow-native and well maintained, but its default
build links a prebuilt libduckdb shared library, and `bundled`
compiles about 41 MB of C++. MotherDuck is reached through the same
library with a static token. SQLite is the same shape and is already
in the binary for accounts.

## Candidate approaches

### A. ADBC in-process, as dbt Fusion and Spice.ai did

Arrow Database Connectivity gives one Rust trait set
(`adbc_core`, `adbc_driver_manager` 0.24, 2026-07) over drivers loaded
as shared libraries. Since ADBC release 24 (2026-07) the Apache project
keeps only Postgres, SQLite and Flight SQL; the warehouse drivers moved
to the ADBC Driver Foundry (adbc-drivers.org), run by Columnar and
installed with its `dbc` tool.

Coverage, 2026-10: Snowflake 1.15 and BigQuery 1.14 (Go, production),
Databricks 0.2 (Go, early; an unreleased native Rust driver sits in
the same repo), Redshift 1.8 and SQL Server 1.7 (binary only, source
private, Columnar's Permissive Binary License), Trino 0.6 (Basic auth
only), MySQL 0.7, ClickHouse 0.1 and Druid 0.1-alpha (both Rust, both
prerelease), Postgres and SQLite from Apache. Athena has no driver
beyond a two-star best-effort repo at dbt-labs. Counted honestly:
production-grade with source, four (Snowflake, BigQuery, Postgres,
SQLite); production-grade binary-only, two; 0.x, five; none, one. The
registry, CDN, code signing and `dbc` installer behind the Foundry
belong to one venture-backed company, Columnar, so this is a
single-vendor dependency wearing a community name.

Auth: OAuth is a driver option on Snowflake, BigQuery, Databricks (U2M
and M2M) and SQL Server (pre-acquired token). Not on Trino, MySQL,
ClickHouse, Druid; Redshift takes IAM.

Performance: the strongest of any option. Arrow end to end, Snowflake
prefetches five result streams, BigQuery reads through the Storage
Read API, Databricks through CloudFetch. The Foundry's own benchmarks
claim 2–3x over the vendor JSON paths on large results.

Against the constraints: the drivers are prebuilt shared libraries of
11–17 MB each, dlopen'd at runtime. The binary itself would grow only
by `arrow` and the driver manager, but it would have to become a
glibc-dynamic executable, which `docs/releasing.md` records as the
thing that broke name resolution and was deliberately abandoned. The
drivers must be present on the host, so installation becomes "the
binary plus a directory of drivers", and the Rust bindings have had a
breaking change every quarterly release since 0.20. The two drivers
behind closed source are two of our thirteen. The API is blocking and
serialised per connection, so each call is a `spawn_blocking`.

Verdict: the best engine and the wrong packaging for a static binary.
Viable only inside a second process (see F).

### B. ODBC through `odbc-api` / `arrow-odbc`

Covers eleven of thirteen with free vendor drivers, thirteen if Trino
and Druid are bought from Starburst or Imply. OAuth tokens pass through
on Snowflake, Databricks and BigQuery's Simba driver. The cost is
unixODBC plus thirteen proprietary `.so` files and their `odbcinst.ini`,
and the data path is row-bound ODBC buffers re-encoded to Arrow in
process, so no columnar transfer. Every embedding objection to A
applies, with worse performance and a worse operator story. Discarded
as a primary; noted as a fallback if a single driver is ever missing.

### C. ConnectorX, DuckDB as a hub, DataFusion as a federator

ConnectorX (0.4.6, 2026-09, MIT) reaches eight of thirteen, has no
Snowflake, Databricks, Athena or Druid, no OAuth anywhere, and pins
an older `arrow`. DuckDB with its community extensions reaches about
nine, by downloading a per-version `.duckdb_extension` shared library
per source at runtime, which forfeits the static binary anyway and
routes every result through DuckDB's vector engine and back out.
DataFusion's table providers cover five natively and the rest by
delegating to ADBC or ODBC, and its federation layer regenerates SQL
from its own plan, which is the job 0sql already does in more dialects.
Each embeds an engine or a planner this project does not need. All
three discarded.

### D. One pure-Rust crate per dialect, all compiled in

Thirteen drivers in the binary: the four wire-protocol crates above,
plus `google-cloud-bigquery` (gRPC via tonic for Storage Read),
`aws-sdk-athena` and `aws-sdk-redshiftdata` with `aws-config`, a
Snowflake crate, a ClickHouse crate, `duckdb` bundled, and reqwest for
the rest. It covers thirteen of thirteen and stays static. But the AWS
SDK crates and tonic each bring a large dependency tree, the Snowflake
crates are fragmented (the one with OAuth returns JSON; the one with
Arrow lacks OAuth; neither has more than 45 stars), and `duckdb`
bundled alone adds minutes to a clean build and tens of megabytes. This
is the shape that fails constraints 1 and 2.

### E. HTTP-first pure Rust: one HTTP client, three wire crates

The observation that makes a thin engine possible: eight of thirteen
warehouses are a JSON HTTP API with a bearer or SigV4 header, and the
server already links hyper and rustls. So the engine is:

- one async HTTP client (reqwest on rustls, or hyper directly since
  it is already linked) driving Snowflake, Databricks, BigQuery,
  Trino, Druid and ClickHouse with hand-written request and response
  types, a few hundred lines each;
- `aws-sigv4` and `aws-credential-types` (two small crates, not the
  SDK) to sign Athena and Redshift Data API calls, with the same
  hand-written request types;
- `tokio-postgres`, `mysql_async` and `tiberius` for the four
  wire-protocol warehouses, Redshift reachable either way;
- `rusqlite`, already present;
- DuckDB behind a cargo feature that is off in the release build, or
  deferred (see below).

Every token-shaped credential the warehouses accept is a header this
engine controls, so OAuth is a token source feeding a header, not a
driver capability to wait for. The pieces that are not headers are
exactly the ones no library solves either: Redshift's IdP federation
ends in STS and a temporary password, Athena is IAM by definition,
Postgres 18 OAUTHBEARER has no released Rust client yet.

Performance: the trade is JSON rows instead of Arrow batches on
Snowflake, Trino, Druid, Redshift and Athena, since those protocols
offer nothing else over HTTP. Databricks offers ARROW_STREAM through
external links and BigQuery offers Arrow through the Storage Read API
(gRPC, which this design skips), so both would start on JSON too. That
is the right trade for this product: the planner emits aggregated
analytical statements whose results are hundreds to tens of thousands
of rows, where per-query latency (connection setup, auth, polling
cadence) dominates and bulk throughput does not. Arrow can be added
later behind the same result trait for the two warehouses that serve
it over HTTP, if a use case for million-row results appears.

Against the constraints: measured below. Coverage is thirteen of
thirteen, with DuckDB the one that costs.

### F. A separate executor process

Keep `zsql` exactly as it is and ship a second binary, `zsql-exec` or
similar, that the server talks to over a local socket. The executor
may then be glibc-dynamic and load ADBC drivers (approach A, Rust), or
be written in Go against the vendor drivers that the ADBC Foundry is
itself built on (gosnowflake, databricks-sql-go, the BigQuery client,
pgx, go-mssqldb), all of which carry OAuth and three of which emit
Arrow. The main binary gains nothing but a client. Steampipe documents
the cost of this pattern, a gRPC hop per call; against warehouse
round-trips of hundreds of milliseconds it is noise.

Against the constraints: it satisfies 1 and 2 by construction and 3
through ADBC or Go, and it is the only route to Arrow everywhere
without compromising the static binary. The costs are a second
artifact to build, sign, install and version, a second language if Go,
and a dependency on the Foundry's governance and its two closed-source
drivers if ADBC. It is the natural second step if E's JSON path ever
proves too slow, because the same executor trait sits in front of
both.

## What every comparable product does

Every open source product that runs SQL against many warehouses live
runs vendor drivers in-process: Cube and Lightdash and Evidence in
Node, Metabase on JDBC, Superset on SQLAlchemy, dbt Core on the Python
connectors, Rill on Go drivers. The two Rust projects with the same
problem, dbt Fusion and Spice.ai, both chose ADBC with Go drivers
loaded as shared libraries, and dbt's docs say why: Go compiles to
well-behaved shared libraries and Java and C# do not. The one sidecar
design, Steampipe, is for API-shaped sources and documents its latency.
Nobody ships a static binary that executes against thirteen warehouses;
that is the constraint set that makes E, rather than A, the fit here.

Three operational lessons recur across Cube, Lightdash and Superset
and apply to any approach:

- Per-user OAuth means a connection pool per identity, not per
  datasource (Cube's `context_to_orchestrator_id`, Superset's per-user
  token table).
- Token refresh must be serialised per credential: Snowflake refresh
  tokens are single-use and Databricks rotates them, and both
  Lightdash and dbt Fusion shipped bugs before adding the lock.
- Snowflake blocks password auth for service users from October 2026,
  so key-pair JWT or PAT is the floor for Snowflake, not an extra.

## OAuth per warehouse

What the protocol allows, independent of any library, so a gap can be
named as the warehouse's or ours.

| Warehouse | Mechanisms | Token on the wire | Lifetime |
|---|---|---|---|
| Snowflake | Snowflake OAuth, External OAuth (any IdP), key-pair JWT, PAT, WIF | bearer header (SQL API) or `authenticator=oauth` (drivers) | access 10 min default, up to 1 h; refresh single-use |
| BigQuery | user OAuth, service account, WIF | bearer header | about 1 h; refresh long-lived |
| Databricks | OAuth U2M, OAuth M2M, PAT, Entra | bearer header | 1 h; refresh rotates, 7-day default TTL |
| SQL Server / Azure SQL | Entra access token, SQL auth | TDS FEDAUTH at connect; driver does not refresh | 60–90 min, pool keyed by identity |
| Trino | OAuth2 JWT, Basic, Kerberos | bearer header, JWT only | IdP's |
| ClickHouse Cloud | JWT from external IdP | bearer header over HTTPS | IdP's |
| Druid | Basic, Kerberos, pac4j OIDC JWT | Basic or bearer header | IdP's |
| Redshift | IAM temp credentials, IdP plugins in AWS drivers only, Identity Center, Data API | SigV4 to the Data API, or temporary password over the wire | 15–60 min passwords |
| Athena | IAM SigV4 only; SAML ends in `AssumeRole` | SigV4 | STS session |
| Postgres | SCRAM; PG 18 OAUTHBEARER; cloud tokens as password | SASL or password | IdP's |
| MySQL | password; cloud tokens via cleartext over TLS; OIDC plugin Enterprise-only | password field | IdP's |
| DuckDB / SQLite | none (MotherDuck static token) | n/a | n/a |

The pattern: on seven warehouses OAuth is a bearer header, on two it is
a temporary password minted from AWS credentials, on two it is a
password field carrying a cloud token. None of that needs a driver
feature; it needs a token source abstraction (static, file re-read per
connection, client-credentials against a token endpoint, refresh-token
exchange) and an identity-keyed pool.

## Measurements

Clean release builds on this machine (12 cores), stripped, each a
minimal `main` that touches the dependency so nothing is dropped.
`m0` is the floor shared by all: tokio, serde_json, rustls.

| Build | Clean release build | Stripped binary | Crates | What it stands for |
|---|---|---|---|---|
| `m0` floor: tokio, serde_json, rustls | 25 s | 0.57 MB | 54 | shared by every option |
| `m1` approach E: floor + reqwest (rustls), tokio-postgres, mysql_async, tiberius, aws-sigv4 | 52 s | 3.35 MB | 275 | the thin engine, all thirteen but DuckDB |
| `m2` approach A: floor + arrow, arrow-ipc, adbc_core, adbc_driver_manager | 73 s | 0.56 MB (dead code dropped; the weight is 11–17 MB per driver on disk) | 148 | the ADBC bindings alone, no driver |
| `m3` DuckDB: floor + `duckdb` bundled | 317 s | 40.8 MB | 178 | the one adapter that costs |
| `m4` DataFusion 52 + arrow-flight with `flight-sql` | 376 s | 99.8 MB | 346 | a Rust local engine with a Flight SQL front door |
| `zsql` today, for scale | 69 s | 14.1 MB (18.2 MB unstripped) | 231 | the whole product |

Read against `zsql`: approach E adds about half a minute to a
seventy-second clean build and about 3 MB to a 14 MB stripped binary,
less in practice because tokio, hyper, rustls and serde are already
compiled in and the scratch crate counts them again. That is a
noticeable cost and not a dramatic one. DuckDB bundled adds five
minutes and 40 MB on its own, roughly tripling both numbers, which is
why it is feature-gated below rather than carried. The ADBC bindings
add forty-eight seconds of compile for `arrow` and would need the
binary to become glibc-dynamic before a single driver loaded.
DataFusion with Flight SQL, measured for the second pass below, is
heavier than DuckDB on both axes: six minutes and a hundred megabytes.

## Recommendation of the first pass

Superseded in part by the second pass below, which widens the brief to
a data plane and reorders the work; kept because it is still the right
answer to the narrow question "run one statement from the binary".

Approach E, in a new `zsql-exec` crate behind a cargo feature, with F
as the documented escape hatch.

- An `Executor` trait: connect from a datasource's connection block and
  a credential, run one statement with a timeout and a row cap, stream
  rows as a small self-describing result type (column names, declared
  types, JSON-like values), cancel. Arrow is not the internal type at
  first; the trait leaves room for a batch variant.
- Eight HTTP adapters on one client, two of them SigV4-signed. Three
  wire adapters on `tokio-postgres`, `mysql_async`, `tiberius`. SQLite
  on what is already there.
- A `Credential` type with token sources: static secret, file path
  re-read on connect, OAuth client-credentials with a token endpoint,
  refresh-token exchange, AWS credential chain. Pools keyed by
  datasource plus identity. Refresh serialised per credential.
- DuckDB: a `duckdb` cargo feature, off in the release build, for
  developers who build it themselves, until a measurement says
  otherwise. Note that the only remote DuckDB is MotherDuck and it
  takes a static token, so the local-file case is the CLI's, not the
  server's.
- Measured gates before merging: release binary growth and clean build
  time against the table above, and the release workflow's static
  assertion still passing.

What it gives up: Arrow transfer, which only two of the eight HTTP
warehouses can serve over HTTP anyway, and which approach F restores
without touching the main binary if it is ever needed.

## To verify before building

- Snowflake SQL API v2 against a real account: partition sizes,
  statement timeout behaviour, whether `resultSetMetaData` types are
  enough to type the rows without the driver protocol.
- Databricks: whether JSON_ARRAY inline at 25 MiB is enough for the
  result sizes we expect, or external links are needed from day one.
- BigQuery: `jobs.query` with `useLegacySql=false` and the
  `timeoutMs` long-poll, versus insert-job-then-poll.
- Trino spooled protocol: whether to opt in (`json+zstd` needs a zstd
  decoder) or stay on the inline encoding.
- Redshift: Data API versus wire protocol as the default, given the
  Data API's 500 MB cap and its async model but its native IAM story.
- `tiberius` maintenance: it was dormant from 2024-07 to 2026-08; the
  new maintainers' first releases are weeks old.
- Whether the release workflow's `cross` build for arm64 musl handles
  `tiberius` and `mysql_async` cleanly (both pure Rust with rustls; it
  should).

## Sources

Reports gathered 2026-10-18 by three research passes; the URLs behind
each claim above:

- ADBC: arrow.apache.org/blog/2026/07/28/adbc-24-release, adbc-drivers.org/drivers, docs.rs/adbc_driver_manager, columnar.tech/blog/introducing-dbc, adbc-drivers.org/blog/2026/02/19/faster-adbc-drivers
- dbt Fusion on ADBC: docs.getdbt.com/docs/fusion/adbc; Spice.ai connectors: spiceai.org/docs/components/data-connectors
- Cube OAuth: docs.cube.dev/admin/connect-to-data/oauth-authentication; Lightdash: docs.lightdash.com/references/workspace/personal-warehouse-connections, PRs #30840 #30844; Superset SIP-85: github.com/apache/superset/pull/30126
- Steampipe architecture: steampipe.io/docs/develop/architecture
- Snowflake: docs.snowflake.com/en/developer-guide/sql-api/authenticating, /handling-responses, /user-guide/oauth-intro, /user-guide/programmatic-access-tokens
- Databricks: docs.databricks.com/api/workspace/statementexecution/executestatement, /dev-tools/auth/oauth-m2m, /oauth-u2m
- BigQuery: docs.cloud.google.com/bigquery/docs/authentication, /reference/rest/v2/jobs/query, /reference/storage
- Trino: trino.io/docs/current/develop/client-protocol.html, /security/oauth2.html, github.com/trinodb/trino/issues/24586
- Druid: druid.apache.org/docs/latest/api-reference/sql-api, /operations/auth
- ClickHouse: clickhouse.com/docs/interfaces/formats/ArrowStream, /concepts/features/security/external-authenticators/jwt, github.com/ClickHouse/clickhouse-rs
- Redshift: docs.aws.amazon.com/redshift/latest/mgmt/data-api.html, /redshift-iam-access-control-native-idp.html, /generating-user-credentials.html
- Athena: docs.aws.amazon.com/athena/latest/APIReference/API_GetQueryResults.html, /ug/unload.html
- Postgres: postgresql.org/docs/18/auth-oauth.html, github.com/rust-postgres/rust-postgres/pull/1378
- MySQL: dev.mysql.com/doc/refman/9.4/en/openid-pluggable-authentication.html, docs.rs/mysql_async
- SQL Server: docs.rs/tiberius/latest/tiberius/enum.AuthMethod.html, learn.microsoft.com/en-us/openspecs/windows_protocols/ms-tds
- Crates: crates.io for tokio-postgres, mysql_async, tiberius, rusqlite, duckdb, clickhouse, snowflake-connector-rs, snowflake-api, trino-rust-client, google-cloud-bigquery, gcloud-bigquery, aws-sdk-athena, aws-sdk-redshiftdata, connectorx, odbc-api, arrow-odbc, arrow-flight

---

# Second pass: the engine as a data plane

Same day. The brief was widened: the engine should eventually
post-process locally (totals, blends across datasources, blends with a
local CSV), reach new kinds of source, and host small reusable OLAP
cubes; and should the executor be written in Python, where every data
vendor has a client? This pass re-weighs the options against that
future and looks further out than the brief did. A first draft of it
was handed to a fresh reviewer with no stake in the conclusions and
told to attack it; where the reviewer changed the position, the text
says so, and where it did not, it says that too.

## What the future state changes

**Arrow becomes the internal type where results meet.** The first pass
chose JSON rows because nothing downstream needed columns. The moment
two result sets are joined, totalled or persisted, the internal type
is a columnar batch, and every local engine speaks Arrow. The reviewer
rightly narrowed this: on the agent path, where a result is hundreds
of rows and goes straight out as JSON, Arrow buys nothing. So: Arrow
inside the engine, JSON at the API edge, and the connectors that
deliver Arrow natively (ADBC's, Databricks external links, BigQuery
Storage Read, ClickHouse ArrowStream, DuckDB) matter only once there is
an engine.

**A local engine stops being a cost to avoid.** The first pass gated
DuckDB off because 40 MB and five minutes was too much for one adapter.
If the engine must blend, total and read CSV, a local SQL engine is the
core and its size is the price of the feature.

**The planner binary and the engine cannot be the same artifact.**
Everything the future wants is heavy, stateful and glibc-happy, which
is exactly what `zsql` must not become. Whatever executes, it is a
second thing, and the interface between the two is the decision that
outlives every other one in this document.

## The planner already has the shape for this

0sql's planner already produces a DAG of sub-queries that it aggregates
apart and stitches on conformed dimensions when measures come from
facts at different grains (`crates/planner/src/dag.rs`). Within one
datasource it renders that DAG as one statement with CTEs. Across
datasources it refuses, and `docs/parked-enhancements.md` already names
the fix: one statement per datasource plus a merge specification the
caller executes. It also already ranks candidate tables by datasource
tier, hot before warm before cold (`crates/planner/src/pruner.rs`).

Those two facts carry most of the future state without a new
intermediate representation:

- **Blends** are leaves in each datasource's dialect plus a merge
  statement in a local engine's dialect that treats each leaf result
  as a table. The `duckdb` dialect exists, so the merge needs no new
  dialect work. A CSV is a leaf whose datasource is a file. Totals are
  a merge step. Period-to-date and relative-period logic that is
  painful in thirteen dialects (parked in the same document) becomes
  one implementation in the merge engine.
- **Cubes** are tables in a hot-tier datasource whose fields are
  declared at a grain; the planner picks them over the warehouse
  whenever a request is answerable at that grain. Building one is a
  planned query written to Parquet; refreshing it is the same query
  with a time filter. The semantic model is the aggregate navigator,
  which is what Cube sells as pre-aggregations, and 0sql can do it with
  grain awareness neither Cube nor Druid has.

**Row-level security is the design constraint, and the first draft got
it wrong.** Policies become `WHERE` filters and `CASE` masks in the
statement, so a leaf run against a warehouse is already filtered for
the context that asked. A cube built once and served to many contexts
is not. The draft proposed retaining the policy columns in the cube and
filtering at merge time. The reviewer is right that this only holds for
additive measures: a distinct count, a ratio, anything aggregated past
the policy's grain cannot be re-filtered after the fact, and a policy
that resolves through a join or a user attribute has no column to
retain. The sound rules are: additive measures may be filtered locally
if the policy's columns are retained at cube grain; non-additive
measures need a cube per policy predicate, or sketches (below) where
the warehouse can emit them. This is the first time 0sql would hold
rows, and it has to be as trustworthy as the warehouse path.

## The question the brief skipped: should 0sql execute at all

The reviewer's strongest point, and it belongs before any engine
choice. "It never connects to your warehouse and never runs a query" is
the first sentence of the README and the sentence an enterprise
security reviewer approves. Three facts bear on whether to spend it:

- The warehouses are absorbing the semantic layer. Snowflake Semantic
  Views feed Cortex Analyst; Databricks Metric Views reached general
  availability on 2026-04-02 and feed Genie. Each is single-platform,
  which is exactly the gap a thirteen-dialect planner fills, and
  executing does nothing to widen that gap.
- The model, not the plan, is what the industry is standardising. Open
  Semantic Interchange released spec 0.1.1 in December 2025, launched it
  publicly on 2026-01-27 and entered the Apache Incubator as Apache
  Ossie on 2026-07-10, with Snowflake, Databricks, dbt Labs, Microsoft
  and some fifty others behind it (there is no "v1.0"; see
  `docs/ossie.md`).
  Importing and exporting Ossie is a larger lever for 0sql than a data
  plane and costs a fraction as much; it was absent from the brief.
- The agent that is 0sql's primary consumer almost always has an
  execution environment already: a Python sandbox, a warehouse MCP
  server, Cortex, Genie. It needs correct SQL with provenance and a
  small result. It does not need 0sql to own a DuckDB.

None of this says never. It says the promise should be kept for the
binary and the hosted app by default, and execution should arrive first
in forms that do not break it. The option space below is ordered by
how much of the promise each spends.

## The option space, widened

The draft framed the choice as a Rust service versus a Python service.
The reviewer added three options the draft never reached, and two of
them are better than anything in it.

**1. Let the customer's engine blend.** Trino and Starburst have
connectors for every one of the thirteen; Databricks Lakehouse
Federation reaches Snowflake, Redshift, BigQuery, Postgres, MySQL and
SQL Server; Snowflake and BigQuery read each other's Iceberg tables.
0sql already has a Trino dialect. A cross-source blend for a customer
with Trino is: qualify each leaf's tables with its catalog name and
render the whole DAG as one Trino statement. No connectors, no rows
held, promise intact. The planner change is small (a datasource may
declare the catalog it is reachable under in a federating datasource)
and it ships the parked feature for the customers most likely to want
it. The draft dismissed DataFusion-as-federator in a sentence and never
considered the customer's own federator at all.

**2. Execute at the edge, in the customer's process.** A Python
package, `pip install zsql`, that calls the planner (the CLI or
app.0sql.io), takes the SQL and runs it in the customer's own Python
through Ibis or ADBC, with a local DuckDB for blends, CSVs, totals and
small cubes as Parquet on the customer's disk. Ibis covers all thirteen
targets (Redshift through its Postgres backend), executes raw SQL to
Arrow, and carries the vendor clients' complete auth matrices
underneath it: Snowflake's connector has every OAuth flow and returns
Arrow batches, Databricks' has U2M, M2M and CloudFetch, BigQuery's has
the Storage Read API, `pyathena` and `redshift_connector` carry AWS's
IdP plugins that exist nowhere else outside JDBC. The binary stays
static and never holds rows; the hosted service never holds
credentials; the OAuth matrix is the vendor's problem; and the data
team gets Python extensibility (a UDF in a merge step, a forecast as a
calculation, a connector for a vendor that only ships Python). This is
what "a Python-based equivalent" should mean, and it is the Python
option a CTO approves.

**3. A thin `--run` in the CLI.** The first pass's approach E, cut to
what a developer's loop needs: SQLite, a DuckDB file behind a feature,
Postgres, MySQL and SQL Server over their pure-Rust wire crates, and
the HTTP APIs where cheap. Feature-gated, measured against the table
above, binary stays static, no hosted exposure.

**4. A self-hosted data plane, later.** A second artifact, run on the
customer's own infrastructure beside `zsql`, that holds connections, a
local engine, a cache and cubes, reached over Arrow Flight. This is the
only shape that gives an agent "ask and get numbers" in one tool call
without the customer's credentials leaving the customer. It is also the
big build: a local engine, twelve connectors or ADBC, identity-keyed
pools, serialised token refresh, cube lifecycle, and the RLS rules
above. It should wait for a customer who asks for it by name.

**5. A hosted data plane on app.0sql.io.** The same thing, holding
customers' warehouse credentials and rows in 0sql's infrastructure for
the first time. One engine process per tenant is the only defensible
isolation (DuckDB has none internally); every Snowflake customer will
ask for egress IP allowlisting; the cache is customer data on 0sql's
disk and a lake reader is customer scans on 0sql's CPU; SOC 2 scope
changes. The memory of a cross-tenant fix at v0.4.0, for a product that
held no rows, is the relevant precedent. Only with a signed customer
and a compliance budget.

**6. A Python data-plane service.** Rejected, for reasons the draft
got partly wrong. The GIL is a strawman: Python 3.14's free-threaded
build is supported, every driver releases the GIL during I/O, and DuckDB
and Arrow do the compute in C++; it would not appear on a profile. The
real costs are supply chain (the Databricks connector alone is over
100 MB installed, and without PyArrow its CloudFetch path is off), a
hosted Python service holding credentials as a second security boundary
to audit, and an operator story of `uvx zsql-exec` beside a static
binary. The draft's idea of a Python spike later ported to Rust does
not survive contact with a shipping product; spikes that work are never
ported. Python's place is option 2, in the customer's process.

**7. A Go data plane.** Discarded. Driver coverage as good as Python's,
a third language, none of Python's data-team upside.

## Which local engine, if a Rust data plane is ever built

Measured on this machine, clean release builds, stripped:

| Engine | Build | Binary | Dialect work | Readers | Composition |
|---|---|---|---|---|---|
| DuckDB bundled | 317 s | 40.8 MB | none, `duckdb` dialect exists | best in class: CSV, Parquet, JSON, Iceberg, Delta | SQL strings over registered Arrow tables |
| DataFusion 52 + Flight SQL | 376 s | 99.8 MB | a fourteenth dialect file | Parquet and CSV native; Iceberg and Delta via `iceberg-rust` (0.10, writes immature, the 0.11 vote failed twice in September) and `delta-rs` | Rust operator library; a remote leaf is a `TableProvider` |

The draft leaned to DataFusion as the spine for its programmability and
its Substrait and Flight SQL doors. The measurement and the review
reverse that. DataFusion costs more on both axes than the engine it
would replace, its Substrait producer had open correctness bugs in
September 2026 (aggregates emitted with an unspecified phase, `ILIKE`
as an undefined function), and its Flight SQL front door serves a
product, "the semantic layer as a database", that Cube built a whole
company on and that BI tools are now bypassing by connecting to
Semantic Views directly. DuckDB is the engine for any Rust data plane:
the dialect exists, the readers are better, and the merge statement is
SQL the planner already knows how to write. DataFusion is justified
only if a remote leaf must be a plan node rather than a registered
table, and nothing in the future state needs that.

## Around the corner, revised

The reviewer graded the draft's bets. The grades were mostly fair.

**Promoted: numeric goldens.** Today the golden suite compares SQL
text per dialect. With a local engine in the dev build, every golden
can be run against the fixture data and compared as numbers, and with
containers in CI the same spec can run on Postgres, MySQL, ClickHouse
and Trino and the results compared across dialects. Novel, risk-free,
needs no product change, and it forces the executor trait into
existence for the CLI alone. This is the first thing to build.

**Added: Apache Ossie.** A semantic model that imports from and exports
to the format Snowflake, Databricks and dbt have agreed on is how 0sql
becomes the planner that sits across the warehouse-native semantic
layers rather than beside them. It is also the answer to "connecting to
new data sources" that costs the least: the model comes in, the planner
already speaks the dialect.

**Added: the competitor is warehouse-native.** Semantic Views and
Metric Views are each one platform, and each is free to that platform's
customers. 0sql's case is correctness across platforms and refusal
before it is wrong; execution does not strengthen that case and a
hosted data plane weakens the sentence that opens it.

**Kept: agent follow-ups from a cached result.** An agent that gets
rows back asks the next question of the same rows. A result kept in
Arrow for the session answers drill, pivot and re-sort in milliseconds
without a second warehouse bill, and the semantic layer is the only
component that can compute the cache key (model version, spec,
security context) correctly. In option 2 this cache lives in the
customer's process; in option 4 it lives in the data plane.

**Kept: cost attribution and routing.** Execution exposes bytes
scanned, credits and latency per field and per table, which no BI tool
can attribute as precisely because none has the model. The parked
field hit counts become real. Routing a request to the cheapest
answerable tier is the hot/warm/cold ranking the planner already does,
extended with observed cost.

**Kept: mergeable sketches.** Distinct counts do not add across cubes
or sources; HyperLogLog and Theta sketches do, and ClickHouse, Druid,
Snowflake and BigQuery can emit them. Parked in
`docs/parked-enhancements.md` as mergeable distinct counts; a cube that
stores sketches stays reusable at coarser grains and under the RLS
rules above.

**Wrong tense: the lakehouse as source.** The draft said every
warehouse now exposes its tables through an Iceberg REST catalog with
credential vending. Snowflake's Horizon exposes only Snowflake-managed
Iceberg tables, not native ones; Glue vends credentials only for
buckets registered with Lake Formation; Postgres, MySQL, SQL Server,
ClickHouse, Druid, Redshift, Athena, SQLite and DuckDB expose no
catalog at all. Reading Iceberg directly reaches the Iceberg-shaped
slice of three warehouses, and in a hosted shape it moves their compute
bill to 0sql's. Right direction, a 2028 source, not a 2026 one.

**Wrong bet: emit plans, not strings.** Substrait is consumed by
DataFusion with the bugs above and by DuckDB only through an extension
that DuckDB Labs no longer maintains and that is not in the
distribution; the spec breaks monthly. Thirteen tested dialect files
are the moat. The interchange that matters is the model (Ossie), not
the plan.

**Demoted: the data plane as a database.** Flight SQL or a Postgres
wire front for Tableau and Power BI is a company, not a feature, and
the BI tools are going to the warehouses' own semantic views.

**Demoted to a demo: in-browser.** DuckDB compiles to WebAssembly and
the playground could blend sample Parquet in the visitor's browser. A
good demo of option 2's shape; not a direction.

## What to build, in order

The draft said "cross-source blend first". The reviewer is right that
it is the rarest need, exercises the hardest things at once, and is
already shippable without an engine by option 1. Revised:

1. **Numeric goldens.** An executor trait in a dev-only crate; DuckDB
   behind a feature; run the golden suite against fixture data and
   compare numbers; then Postgres and MySQL in CI containers for
   cross-dialect comparison. No product change, no promise spent.
2. **Federated blends through the customer's engine.** A datasource
   may declare the catalog name it is reachable under in a federating
   datasource (Trino, Starburst, Databricks); the planner renders the
   cross-datasource DAG as one statement there. Ships the parked
   feature with zero connectors.
3. **The Python client.** `pip install zsql`: plan through the CLI or
   server, execute through Ibis or ADBC, DuckDB for CSV blends, totals
   and Parquet cubes on the customer's side, the session result cache
   for agent follow-ups. Where blending and extensibility live, in the
   customer's process. This is the Python equivalent, in the form that
   keeps the promise.
4. **Ossie import and export**, in parallel, as the "new data sources"
   lever that costs least.
5. **`zsql sql --run`** in the CLI for local and wire-protocol sources,
   feature-gated, measured.
6. **A self-hosted data plane** on DuckDB, over Arrow Flight, only when
   a customer asks for one-call "ask and get numbers" on their own
   infrastructure. A hosted one only with a signed customer and a
   compliance budget.

## Where this document and its reviewer still disagree

Two places, recorded so the decision can be made with both in view.

The reviewer would keep "never runs" as a near-permanent property and
treat options 4 and 5 as unlikely ever to be worth it. This document
holds that the agent's one-call experience is a real product pull and
that a self-hosted data plane on the customer's infrastructure keeps
the sentence's substance (0sql the company never sees the data) while
giving the agent the call it wants. That is a bet on how much agents
end up driving adoption, and it should be decided by watching what the
MCP users ask for over the next two quarters, not now.

The reviewer puts the Python client ahead of the CLI's `--run`. This
document agrees on priority and notes the dependency the other way:
the executor trait and result type that the numeric goldens force into
existence in Rust should define the shape the Python client mirrors,
so the two never diverge on what a result, a credential or a cube is.

## Sources added in this pass

- Apache Ossie: ossie.apache.org, getdbt.com/blog/osi-is-now-apache-ossie, snowflake.com/en/blog/apache-ossie-open-semantic-interchange-incubator
- Warehouse-native semantic layers: colrows.com/blogs/snowflake-semantic-views-vs-databricks-metric-views, paradime.io/blog/dbt-semantic-layer-vs-snowflake-semantic-views
- Ibis backends: ibis-project.org/presentations/overview, ibis-project.org/backends/snowflake
- Python connectors returning Arrow: docs.snowflake.com/en/developer-guide/python-connector/python-connector-api, docs.databricks.com/aws/en/dev-tools/python-sql-connector
- Free-threaded Python: PEP 779; github.com/duckdb/duckdb-python/pull/76
- Iceberg catalogs: cloudyard.in/2026/10/horizon-rest-catalog-reading-snowflake-managed-iceberg-tables-from-pyiceberg, dremio.com/blog/preview-of-upcoming-apache-iceberg-1-12-and-iceberg-rust-0-11
- Substrait status: github.com/duckdb/duckdb/discussions/4771, github.com/apache/datafusion/issues/25368, github.com/apache/datafusion/issues/25100
- Trino encodings, 484: trino.io/docs/current/develop/client-protocol.html
- Snowflake password block: bleepingcomputer.com/news/security/snowflake-ends-service-account-passwords-now-comes-the-hard-part
- ADBC Foundry ownership: siliconangle.com/2025/10/29/columnar-launches-redefine-data-connectivity-arrow-powered-adbc-drivers; github.com/adbc-drivers/redshift; github.com/adbc-drivers/mssql
- musl dlopen: github.com/telekom/sutura/pull/1146
- Federation: snowflake.com/en/blog/bidirectional-interoperability-snowflake-horizon-databricks; Databricks Lakehouse Federation docs; trino.io/docs/current/connector.html

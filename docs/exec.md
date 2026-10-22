# Query execution: the `exec` endpoint and the plane behind it

A survey, written 2026-10-21 on branch `exec` and set aside the same day: the plan of record is `docs/introspection.md`, and execution is future matter. It rests on the survey in
`docs/execution-engine.md`, which was paused on 2026-10-18 after
weighing thirteen adapters against a static binary; that document
answers "which library connects to Snowflake". This one answers a
different and larger question: what has to exist for 0sql to accept a
spec, run it, and hand back rows without losing the box, the tenant
boundary, the warehouse bill, or the correctness claim that is the
whole product.

The brief: `POST /projects/{uid}/branches/{branch}/exec`, the same body
`/sql` takes, the statement run against the target datasource, rows
back. The brief is four lines. Everything below is what those four
lines actually require.

Phases are at the end and are the checklist; tick them off here.

## The shape of the answer, stated first

1. **Execution is a second artifact, not a feature of `zsql`.** The
   planner binary stays static, driverless and incapable of connecting.
   `zsql-exec` is a separate process that may be glibc-dynamic, may
   bundle DuckDB, may hold connections and megabytes. Binary-size
   isolation and *resource* isolation are the same decision, and the
   second reason is the stronger one: the thing that kills a planner
   under load is the executor's memory, so the kernel must be able to
   kill the executor alone.
2. **`exec` is a job, not a response.** Submit, wait briefly, then poll
   or stream. A handler that blocks on a warehouse for two minutes is
   how a one-process server dies, and Cloudflare answers `524` at a
   hundred seconds whatever we do.
3. **Every execution is bounded four times** — by the warehouse, by the
   statement, by the streaming loop, and by the kernel — because each
   of the first three is something a warehouse can be found not to
   honour.
4. **The first thing built is not the endpoint.** It is numeric
   goldens: the executor trait proven against fixture data in DuckDB,
   with no product surface, no credentials and no promise spent. It is
   also where this plan expects to find bugs; it has already found one
   (below).
5. **Hosted execution on app.0sql.io is last and is a business
   decision**, gated on a named customer and a compliance budget, not
   on the engineering being ready.

## What a datasource does not carry today

`zsql_semantic::Datasource` is `id`, `uid`, `name`, `adapter`, `tier`,
`description`, `extra_query_params`, `query_timeout`. There is nothing
to connect with, and that is not an oversight to fix in place.

**The model and the connection are different objects with different
owners.** The model arrives in a deployed tar.gz from anyone with
`Write` on the project, is loaded by the confined loader as
`Trust::Untrusted`, is echoed back in branch summaries, and lives in
git. A secret must never be any of those things. So:

- `datasources.yml` keeps declaring the *logical* datasource: adapter,
  tier, and now one new optional key, `connection: <name>`, naming a
  connection the operator has registered. Nothing secret, nothing
  host-shaped.
- A **connection registry** lives beside the accounts store, written
  through `POST /connections` (Owner only), never through a deploy. It
  holds adapter, host, port, database/catalog/schema, warehouse or
  cluster, and a **credential reference** — never a credential.
- Credential references resolve at connect time: `env:NAME`,
  `file:/path` (re-read per connect, which is how Kubernetes projected
  tokens and IRSA work), `exec:/path/to/cmd` (a token-minting hook,
  which is how every customer's existing secret tooling gets in for
  free), `oauth:` (client-credentials or refresh exchange against a
  token endpoint), `aws:` (the credential chain, for SigV4 to Athena
  and the Redshift Data API). Inline secrets are accepted only in the
  self-hosted single-tenant mode and only from a file mode-0600, and
  are stored encrypted with a key from `ZSQL_SECRET_KEY`.
- Schema bump on the accounts database (3 → 4): `connections`,
  `connection_grants`, `exec_jobs`, `query_log`. The store already
  invalidates its auth cache on every audit row; connection changes
  join that path so a revoked connection stops at once.

**Three credential modes, declared per connection.** This is the
decision that governs caching, pooling and row-level security
together, so it is named once and referred to afterwards:

| mode | who the warehouse sees | who applies row-level security | pool key |
|---|---|---|---|
| `service` (default) | one identity for every caller | 0sql, in the statement | connection |
| `delegated` | the caller, from a token on the request | the warehouse | connection + token identity |
| `impersonated` | the caller, via a role or session parameter from the security context | the warehouse | connection + context identity |

`service` is the only mode that needs no per-user token plumbing, and
it is the mode in which 0sql's policies — already compiled into `WHERE`
filters and `CASE` masks — are the enforcement. The other two exist
because large customers have warehouse-native RLS they will not give
up, and because a security reviewer who sees one service account
reading every row will ask for them.

**Execution is its own privilege.** `Verb` is `Read`, `Write`, `Owner`
today, and `exec` must not be `Read`. Planning is free; executing
spends the customer's warehouse credits, which makes a leaked read key
a billing incident rather than a metadata one. Add `Verb::Exec`,
granted per project and per query key independently of `Read`, default
off. The MCP `exec` tool is exposed only when the key carries it, and
`ZSQL_MCP_TOOLS` can withhold it globally.

## Why `exec` cannot be a response

A warehouse query is seconds to minutes. The server is one process with
a measured ceiling of 42,700 requests a second and a 30-second request
deadline (`docs/rate-limiting.md`), and Cloudflare gives an origin a
hundred seconds before answering `524` itself, with no retry hint the
caller can act on. So the contract is a job with a short synchronous
window on the front of it:

```text
POST   .../exec                  {"spec": {...}, "context": {...}, "wait_ms": 2000, "max_rows": 10000}
                                 → 200 with rows, if it finished inside wait_ms
                                 → 202 {"job":"xj_...","state":"running","poll":"..."}
GET    .../exec/{job}            state, stats, and the first page when done
GET    .../exec/{job}/rows?cursor=…&limit=…   pages, Arrow IPC or NDJSON by Accept
DELETE .../exec/{job}            cancel, propagated to the warehouse
GET    .../exec                  this principal's recent jobs
```

- **`wait_ms` defaults to 2000 and is capped at 10000.** Most planned
  analytical statements against a warm warehouse land inside it, so the
  common case is one round trip and the job machinery is invisible.
- **`Idempotency-Key` on submit** dedupes a client retry for five
  minutes, and the same key is passed to the warehouse where its API
  takes one (Snowflake `requestId`, a deterministic BigQuery `jobId`).
  Without this, every client timeout-and-retry doubles the bill, which
  is the most expensive bug this endpoint can ship with.
- **Cancel must reach the warehouse**, not just drop our future. Per
  adapter: Snowflake `POST /statements/{h}/cancel`, BigQuery
  `jobs.cancel`, Databricks `DELETE /statements/{id}`, Trino `DELETE`
  on the `nextUri`, Athena `StopQueryExecution`, Redshift
  `CancelStatement`, ClickHouse `KILL QUERY` by the `query_id` we set,
  Postgres `CancelToken`, MySQL `KILL QUERY`, SQL Server the TDS
  attention signal, DuckDB `interrupt`. An adapter that cannot cancel
  does not ship; a cancel path that is never exercised is not a cancel
  path, so the chaos suite exercises every one.
- **Client disconnect is a cancel.** Axum gives us the dropped future;
  a `CancellationToken` carries it to the adapter. Otherwise an agent
  that gives up leaves the warehouse running.
- **Streaming is the optimisation, not the contract.** With
  `Accept: application/x-ndjson` rows go out as they arrive under
  chunked transfer. It is still bounded by the proxy's ceiling, so the
  job model stays primary and streaming is for the self-hosted and
  Python-client paths where no proxy intervenes.
- **MCP gets `exec` and `exec_result`.** `exec` is the bounded-wait
  call with a hard default row cap (1,000 rows, 1 MB), because an agent
  that receives 100,000 rows has lost its context window, not gained
  data. Over the cap, the tool returns the first page, the row count,
  the job handle and a sentence telling the model to narrow the spec or
  page — refusing loudly beats truncating quietly, which is the same
  principle as the planner's refusals.

## Resource governance

The brief's own warning. A single `exec` can exhaust the box five
different ways — CPU on decode, heap on the result, file descriptors on
pools, the warehouse's own concurrency slots, and the customer's
money — and they fail differently, so each gets its own bound. Numbers
below are starting points to be measured on the host the server runs
on, as `docs/rate-limiting.md`'s were.

### 1. Admission, and why `exec` is not `plan`

A new cost class. `plan` gets `cores × 4` permits because a plan is 70
microseconds of CPU; an exec holds a connection, a result buffer and a
warehouse slot for its whole life, so its limit is not CPU at all.

| bound | default | why |
|---|---|---|
| global exec permits | 32 | I/O-bound; the real limits are the two below |
| per connection | 4 | warehouses queue or throttle: Athena's DML concurrency is a low per-account quota, Snowflake queues past the warehouse's slots, BigQuery meters slots |
| per account | 8 | fairness, as the token buckets are fairness |
| queue depth | 256 per account | past it, `503 Busy` with `Retry-After`, never an unbounded queue |
| queue deadline | 30 s | distinct from the run deadline, and attributable: "we never started" is a different answer from "it ran too long" |

The queue is **not FIFO**. One tenant submitting five hundred jobs must
not push another tenant's single job behind them, so selection is
round-robin over per-account queues with a weight, which is the
cheapest thing that cannot starve. Two priorities: `interactive`
(an API or agent call) and `background` (cube refresh, warm-up), with
background yielding to interactive and capped at a fraction of the
permits.

### 2. Bounds pushed into the warehouse

The cheapest limit is the one the warehouse enforces before it spends
anything.

- **A row cap is always injected**, whether or not the caller sent a
  limit, dialect-correctly (see the bug below). The caller's own limit
  wins when it is smaller.
- **A statement timeout is always sent**: Snowflake
  `STATEMENT_TIMEOUT_IN_SECONDS`, Postgres `statement_timeout`, MySQL
  `MAX_EXECUTION_TIME`, BigQuery `jobTimeoutMs`, Databricks
  `wait_timeout`/`on_wait_timeout`, Trino `query_max_run_time`,
  ClickHouse `max_execution_time`, Druid's `timeout` context, SQL
  Server the login/query timeouts, Athena and Redshift their own work
  group and session settings. Derived from the job deadline minus a
  margin, so the warehouse gives up before we do and the error names
  the warehouse.
- **A bytes-billed cap where one exists.** BigQuery's
  `maximumBytesBilled` is the single most valuable control in this
  whole design, because it refuses the job rather than running it and
  charging for the refusal. Nothing else in the thirteen offers the
  same; the rest get the pre-flight below.
- **A pre-flight estimate where it is free**: BigQuery's dry run is
  free and exact, Snowflake's `EXPLAIN` and Trino's
  `EXPLAIN (TYPE IO)` are cheap estimates. Above a per-account
  threshold, refuse before submitting, with the estimate in the
  refusal. This is the control a data platform owner will ask for by
  name.
- **Every statement is tagged** with the account, project, spec hash
  and job id, through whatever the warehouse offers (Snowflake
  `QUERY_TAG`, BigQuery labels, Databricks statement parameters,
  ClickHouse `log_comment`, a leading SQL comment elsewhere). It makes
  a 0sql job findable in the warehouse's own query history, which is
  the first thing a customer debugging a bill will want.

### 3. Bounds in the streaming loop

Defence in depth, because a warehouse that ignores a timeout or returns
a column of 10 MB strings is a thing that happens.

- `max_rows`, `max_cells` (default 5,000,000), `max_bytes` on the
  decoded result, and a wall deadline, all checked per batch.
- Exceeding any of them **cancels upstream and refuses** with a class
  that names the cap and the value reached. It does not truncate. A
  truncated result with a flag on it is a number an agent will average,
  and silent wrongness is the one failure mode this product exists to
  prevent.
- The one exception is the MCP tool's page, which is an explicit,
  labelled first page of a job the caller can continue — a different
  thing from a truncated total.

### 4. Memory, accounted globally

Per-job caps do not bound a process; `N` jobs at the cap do. So there
is one byte budget for all in-flight results, held as a reservation a
job acquires before it starts and releases on every exit path. A job
that cannot reserve queues or is refused — never admitted to
overcommit.

- Default budget: 1 GB or a quarter of the machine's memory, whichever
  is lower.
- Above a per-job threshold (64 MB) the result **spills** to an
  Arrow-IPC or Parquet file under the data directory, and pages are
  served from it. Spilling is not a special case bolted on: it is the
  same mechanism that gives pagination, the result cache and the
  session cache, so it is built once and used by all three.
- A reservation leak is worse than a bug, so a test runs ten thousand
  jobs through every exit path — success, refusal, cancel, warehouse
  error, panic in the decoder — and asserts the accounting returns to
  exactly zero.

### 5. The kernel, as the backstop

The layer that holds when the accounting is wrong, which eventually it
will be.

- `zsql-exec` runs as its own systemd unit with `MemoryMax`,
  `MemoryHigh`, `CPUQuota`, `TasksMax` and `LimitNOFILE`, built from
  `ops/zsqld.service`, which is already hardened. An OOM kill then
  takes the executor and the planner keeps serving `/sql`.
- `RLIMIT_FSIZE` bounds spill files, `RLIMIT_NOFILE` bounds pools.
- The server treats a dead executor as an open circuit: in-flight jobs
  fail with a clean class, new submits are refused, and the unit
  restarts. `/healthz` reports the executor separately, because "the
  planner is up and execution is down" is a true and useful state.

### 6. Failure isolation between datasources

- A **circuit breaker per connection**: five consecutive failures or a
  sustained timeout rate opens it for a cooldown, so a dead warehouse
  stops consuming permits with connects that will not complete.
- **Connect timeout separate from query timeout** (10 s), because a
  warehouse that is slow to accept connections otherwise burns the
  whole job deadline before any SQL is sent.
- **Retries only on pre-submit, idempotent failures**, with jittered
  backoff and a retry budget as a fraction of the request rate.
  Retrying a statement that may already be running is how you double a
  bill; retrying without a budget is how a slow dependency becomes an
  outage.
- A **pool per identity** (the mode table above), with an idle reaper,
  a maximum age, and **refresh serialised per credential**: Snowflake's
  refresh tokens are single-use and Databricks rotates them, and the
  survey records two products shipping this bug before adding the lock.

### 7. The timeout hierarchy, stated once

Client wait ≤ job deadline, job deadline − margin = statement timeout,
connect timeout well inside both, and every layer's own deadline
strictly smaller than its caller's. Stated once here so that whichever
one fires, the error attributes correctly, and so nobody adds a fourth
with a larger number.

### 8. Money, as a resource

The resource a customer notices first.

- Per-account budgets in bytes scanned and credits, per hour and per
  month, charged **after** the fact from what the warehouse reports
  (BigQuery `totalBytesBilled`, Snowflake's query history, Databricks)
  and enforced as a refusal on the next submit. An exact pre-charge is
  impossible; a fast soft limit is what every cloud does and is enough.
- The query log carries bytes, credits, queue time, run time and cache
  outcome per job, which makes the attribution below possible.

## The result

**Arrow inside, JSON at the edge.** The internal type is an Arrow
batch, because the moment a result is merged, totalled, cached or
spilled it is columnar, and every local engine speaks Arrow. The agent
and API path serialises to JSON, because a hundred rows going to a
model gains nothing from Arrow and the planner's statements are
aggregated: hundreds to tens of thousands of rows, where per-query
latency dominates and bulk throughput does not.

JSON shape is columns plus row arrays, not row objects — half the bytes
and no loss:

```json
{"columns":[{"name":"department","type":"string"},{"name":"sales","type":"decimal(18,2)"}],
 "rows":[["Produce","182344.19"]],
 "stats":{"rows":1,"bytes_scanned":41238528,"queue_us":1204,"run_us":831442,"cache":"miss"},
 "datasource":"warehouse","adapter":"snowflake","job":"xj_01HQ..."}
```

**Types are the part that looks easy and is not.** Thirteen type
systems to one logical set, and two rules that are not negotiable:

- **Decimals are never `f64`.** Money that round-trips through a double
  is wrong in the last place, and this product's claim is correctness.
  Decimal128 inside, a string in JSON.
- **Integers beyond 2^53 are strings in JSON**, because a JavaScript
  caller silently rounds them otherwise. Pinned in the encoder and
  fuzzed, along with null, NaN, infinity, and timestamps with and
  without a zone.

**And the differentiator nobody else has: the model declares the
type.** `Field.data_type` says what a field is supposed to be; the
warehouse says what it returned. Comparing the two turns a class of
silent model-drift bug — a column retyped in the warehouse, a renamed
physical column now resolving to something else — into a reported
mismatch. No BI tool can do this, because none of them holds the
declaration.

## Caches, three of them, not one

Conflating these is how a semantic layer leaks data across users.

**1. The result cache.** Keyed on the model snapshot hash, the rendered
SQL text, the datasource uid, and the freshness of every table the plan
touched. The model already carries the invalidation hook:
`Table.data_updated_at` and `Table.clear_cache_on_data_update` are in
the schema today. TTL per tier, hot shorter than cold.

The security rule, which must be written in the code next to the key:
when the mode is `service`, policies are already compiled into the
statement, so **the SQL text is the security context** and the key is
safe. When the mode is `delegated` or `impersonated`, the warehouse
applied the filtering and the identity is *not* in the text, so the key
must include the identity. One line of reasoning, two code paths, and a
test per mode that proves two different contexts never share an entry.
The memory of a cross-tenant fix at v0.4.0 is why this gets its own
suite in the isolation tests rather than a comment.

**2. The session result cache**, for the thing agents actually do: ask,
then ask again about the same rows. A kept Arrow batch answers drill,
re-sort, top-N and pivot in milliseconds with no second warehouse bill.
The semantic layer is the only component that can decide whether a new
spec is answerable from a held result — it knows the grain, the
measures' additivity and the policies — so this is 0sql's to do and
nobody else's. Bounded by the same byte budget, evicted by LRU, scoped
to the key and context that created it.

**3. Pre-aggregations.** A planned query materialised to Parquet at a
declared grain, which the pruner then prefers because it already ranks
candidate tables by tier and cost (`crates/planner/src/pruner.rs`). A
cube is a table in a hot-tier datasource; selection is machinery that
exists. What is new is the lifecycle (build, refresh with a time
filter, invalidate) and the row-level-security rules the survey's
reviewer corrected:

- additive measures may be filtered locally **only** if the policy's
  columns are retained at cube grain;
- non-additive measures (distinct counts, ratios, anything aggregated
  past the policy's grain) need a cube per policy predicate, or
  mergeable sketches;
- a policy that resolves through a join or a user attribute has no
  column to retain and gets no cube at all.

Default, until there is evidence to loosen it: **cubes serve only specs
that resolve to no policy.** The first time 0sql holds rows it has to be
as trustworthy as the warehouse path, and the conservative default
costs a feature, not a customer.

## Post-processing, blends and federation

The OLAP-engine half, and the part that makes execution worth more than
a convenience.

**The planner is already shaped for it.** It builds a DAG of
sub-queries, aggregates them apart and stitches them on conformed
dimensions when measures sit at different grains
(`crates/planner/src/dag.rs`), and renders that DAG as one statement
with CTEs inside one datasource. Across datasources it refuses, and
`docs/parked-enhancements.md` already names the fix: one statement per
datasource plus a merge specification the caller executes. With an
executor, the caller is us.

**First, the version with no connectors: push federation down.** Trino,
Starburst and Databricks Lakehouse Federation reach these warehouses
already, and 0sql has a `trino` dialect. Let a datasource declare the
catalog name it is reachable under in a federating datasource, and a
cross-datasource DAG renders as one statement there. This ships the
parked feature with zero drivers, zero rows held and the promise
intact, for the customers most likely to want blends. It should be
built before any local merge engine, because it is smaller and it is
free.

**Then, DuckDB as the merge engine** in `zsql-exec`, for customers with
no federator: each leaf's result registered as an Arrow table, the
merge rendered in the `duckdb` dialect that already exists. What that
unlocks, beyond blends:

- grand totals, subtotals and percent-of-total as a merge step rather
  than thirteen dialect implementations;
- period-to-date and relative-period logic — parked precisely because
  it is painful in thirteen dialects — as one implementation;
- a CSV or Parquet file as a leaf, which is the most-asked-for thing in
  every semantic layer's issue tracker;
- mergeable distinct counts via HyperLogLog or Theta sketches, which
  ClickHouse, Druid, Snowflake and BigQuery can emit, keeping a cube
  reusable at coarser grains.

DataFusion is not the engine here. The survey measured it at 376 s and
99.8 MB against DuckDB's 317 s and 40.8 MB, it would need a fourteenth
dialect file, and its Substrait producer had open correctness bugs in
September 2026. DuckDB's dialect already exists and its readers are
better. The only thing that would justify DataFusion is a remote leaf
having to be a plan node rather than a registered table, and nothing
here needs that.

**What 0sql must never build: a shuffle.** Distribution of *compute*
belongs to the warehouse, or to DuckDB on one box. 0sql distributes
*jobs*: see scale-out below. A semantic layer that grows a distributed
execution engine has changed products.

## Model verification, and data tests

Two capabilities that only exist once something can run, both worth
more than they cost, and neither in the brief.

- **`zsql check --run`**: one trivial, bounded statement per table
  (`SELECT <columns> FROM <table> WHERE false`, or the adapter's
  cheapest equivalent) to prove every physical name and column exists
  and has the declared type. A hand-authored model's commonest defect
  is a typo in a physical name, and nothing catches it today — not the
  loader, not the tests, not the goldens. This turns a runtime failure
  into an authoring-time one.
- **Data tests**: `tests/*.yml` already run in a deploy, asserting on
  plans. With an executor they can assert on *numbers* — a known total,
  a row count, a non-null rate, a referential check — which is the dbt
  test surface, available here with the semantic model's vocabulary
  instead of raw SQL.

## Egress: `exec` is an SSRF engine unless told otherwise

A connection names a host, and in hosted mode the host comes from a
customer. That makes the executor a request-forging primitive, and this
is the vulnerability class the rest of the design does not address.

- **Per-account host allowlist**, required in hosted mode.
- **RFC1918, loopback, link-local and the cloud metadata address
  (169.254.169.254) refused by default**, with an explicit opt-in for
  the self-hosted case where the warehouse genuinely is on a private
  network.
- **Resolve once, connect to the resolved address, re-check it against
  the policy**, so DNS rebinding cannot move a permitted name onto a
  forbidden address between check and connect.
- **TLS verification is mandatory and there is no flag to disable it.**
  Every "insecure" option a driver offers is turned off explicitly, and
  a test asserts it, because a self-signed warehouse is a support
  conversation and a disabled verifier is a breach.
- **Port allowlist**, and a **static egress address** for the hosted
  plane, because every Snowflake customer will ask to allowlist us.
- Connection strings are never echoed. A refusal says "connection
  `warehouse` refused: host not permitted", never the host, because
  error messages are an exfiltration channel in a multi-tenant system.

## Correctness: the only reason to build this

Execution's first use is not a product feature. It is proof that the
thirteen dialect files are right, which is the actual moat.

- **Numeric goldens.** The golden suite compares SQL text per dialect
  today. With a local engine in the dev build, every golden runs
  against fixture data and is compared as **numbers**. Goldens are
  never edited, so this is an additive expectation file beside the
  existing ones, and `zsql-devtool golden` grows a `--run` mode.
- **Cross-dialect equivalence in CI.** The same spec against DuckDB,
  Postgres, MySQL, ClickHouse and Trino in containers, results compared
  across dialects. This finds the bugs text goldens structurally
  cannot: week-start conventions, null ordering, decimal rounding,
  timestamp zones, integer division. CI has no containers today, so
  this is a new workflow job, kept off the pull-request path if it is
  slow and run on main and nightly.
- **A chaos suite.** For each adapter: the warehouse dies mid-stream,
  the result arrives at one byte a second, ten gigabytes come back when
  the model said a hundred rows, the connection drops after submit, a
  page is malformed, a cancel is ignored. Each must end in a clean
  refusal with every resource released. This is the suite that decides
  whether the governance above is real.
- **Load.** The measured plan ceiling (42,700 req/s with accounts on)
  must not regress, and execution at full tilt must cost plan
  throughput less than the 20% that 508 concurrent deploys cost.

## Observability and cost attribution

- One span per job, carrying the warehouse's own query id, through
  `tracing` with an optional OpenTelemetry exporter.
- A **query log** per account: spec hash, SQL hash, datasource, rows,
  bytes, credits, queue time, run time, cache outcome, refusal class.
  With retention and an export, this is both the debugging tool and the
  usage report.
- **Cost attribution per field and per table**, which only the
  component holding the model can compute: "this measure is 80% of your
  BigQuery bill". It also makes the parked field-hit-count ranking real
  from data that now exists.
- **Routing by observed cost.** The pruner ranks candidates by tier and
  cost today; with measured latency and bytes per table it can prefer
  the cheapest *answerable* tier. That is a genuine engine capability
  and it arrives nearly free from the query log.

## Scale-out, when one process is not enough

Not phase one, but the shape should not be precluded by phase one.

- The job registry moves from memory into the storage trait that
  `docs/accounts-and-access.md` already names for multi-instance work,
  so any instance can report on any job.
- Spill and cached results move to `object_store`, so a page can be
  served by an instance that did not run the job.
- The session cache stays node-local, with the job handle carrying a
  hint so a follow-up prefers the node holding the batch — and
  correctness never depends on landing there.
- The admission limiter becomes the shared limiter that multi-instance
  rate limiting needs anyway. One mechanism, two users.

## The bug this investigation already found

`Plan::sql_with_limit` appends `"\nLIMIT {n}"` to the statement, and
`crates/planner/src/dag.rs` and `crates/planner/src/sql/builder.rs` do
the same in two more places. There is no dialect indirection on any of
them, and `dialects/sqlserver.yml` has no limit, offset or fetch
template — because SQL Server has no `LIMIT`. A spec carrying a limit
against a `sqlserver` datasource therefore plans to a statement that
will not parse.

Nothing catches it: the goldens compare text, and nothing runs. It is
exactly the class of defect numeric goldens exist to find, found before
they were built, and it is the reason the row cap in §2 above has to go
through the dialect rather than through `format!`.

The fix is a dialect-rendered row limit (`LIMIT`, `TOP`,
`FETCH FIRST n ROWS ONLY`) with a case per adapter in the golden suite.
It is small, it is independent of everything else here, and it should
be its own commit on this branch before any executor exists.

## Phases

- [ ] **Phase 0 — the trait, and numeric goldens.** `crates/exec`: an
      `Executor` trait (connect, run with a deadline and caps, stream
      batches, cancel), the Arrow result type, the JSON encoder with
      its decimal and big-integer rules, SQLite and DuckDB behind cargo
      features off by default. `zsql-devtool golden --run` against
      fixture data, compared as numbers. The dialect-rendered row limit
      and its goldens. No endpoint, no credentials, no promise spent.
      *Gates:* release binary and clean build time unchanged; the
      release workflow's static assertion still passes; numeric
      goldens green on DuckDB.
- [ ] **Phase 1 — `zsql sql --run`, feature-gated.** The developer
      loop: SQLite, DuckDB, Postgres, MySQL, SQL Server over the
      pure-Rust wire crates, credentials from `env:`/`file:`, row and
      time caps, cancel on interrupt, `zsql check --run`, data tests.
      *Gates:* binary growth and build time measured against
      `docs/execution-engine.md`'s table; the feature stays off in the
      release build until a measurement says otherwise.
- [ ] **Phase 2 — the exec plane, self-hosted only.** `zsql-exec` as a
      second artifact over a local socket; the connection registry and
      schema bump; the three credential modes with identity-keyed pools
      and serialised refresh; `Verb::Exec`; the job model and all four
      bound layers; the byte budget and spill; the result cache with
      its per-mode security rule; the query log; egress policy; the
      chaos and leak suites; the eight HTTP adapters and three wire
      adapters. `ZSQL_EXEC=off` by default; `/exec` answers 404 when it
      is off. *Gates:* the chaos suite green per adapter; zero
      accounting leak over ten thousand jobs; plan throughput
      regression inside the agreed bound; the isolation suite extended
      with a case per credential mode.
- [ ] **Phase 3 — blends.** Push-down federation through the customer's
      engine first (a catalog declaration, no connectors), then the
      DuckDB merge for everyone else: cross-datasource leaves, grand
      totals, percent-of-total, period-to-date, a CSV leaf.
- [ ] **Phase 4 — pre-aggregations and the session cache.** Cubes to
      Parquet with a lifecycle, grain-aware selection through the
      existing pruner, the row-level-security rules above with
      policy-free specs as the default, mergeable sketches, agent
      follow-ups from a held result.
- [ ] **Phase 5 — hosted execution on app.0sql.io.** A process per
      tenant, an egress proxy with a static address, customer
      credentials under a KMS, SOC 2 scope, and the README sentence
      rewritten. Gated on a named customer and a compliance budget, not
      on readiness.

Cross-cutting and in every phase: the cost budget, the query log, the
chaos cases for whatever that phase added, and the documentation
sentence that says what connects to what.

## The sentence in the README

"It never connects to your warehouse and never runs a query" is the
first paragraph of the README and the sentence an enterprise security
reviewer approves. It is not spent lightly, and phases 0 and 1 do not
spend it at all. From phase 2 the honest form is: **the planner never
connects; execution is a separate component you run, on your
infrastructure, with credentials we never see.** That is still a
sentence a reviewer approves, and it stays true through phase 4. Only
phase 5 makes it false, which is one more reason phase 5 needs a
customer's name on it.

## To verify before building

From the survey, still open: Snowflake SQL API v2 partition sizes and
whether `resultSetMetaData` types are enough to type rows; Databricks
inline `JSON_ARRAY` at 25 MiB versus external links; BigQuery
`jobs.query` long-poll versus insert-and-poll; Trino's spooled
protocol; Redshift Data API versus the wire protocol; `tiberius`
maintenance after its 2024–2026 lull; `cross` arm64-musl builds of
`tiberius` and `mysql_async`.

New, and each one can change a decision above:

- Does every adapter's cancel actually stop the **spend**, or only the
  wait? Measured against a real bill, per warehouse. If cancel does not
  stop spend, the pre-flight estimate becomes mandatory rather than
  optional.
- `maximumBytesBilled`: does BigQuery refuse before charging, in every
  job shape we emit?
- Does DuckDB's bundled build work under the toolchain the release
  workflow uses, and does the `MemoryDenyWriteExecute=yes` already in
  `ops/zsqld.service` stay compatible with it?
- Does NDJSON streaming survive Cloudflare's hundred seconds, or does
  the job model have to be the only path in hosted mode?
- Is one byte budget enough, or does decode transiently need a multiple
  of the result size per adapter? Measured per adapter, because the
  answer sets the budget's safety factor.
- Do the fixture projects have data to run numeric goldens against, or
  does phase 0 start by generating it? (`data/` is empty; the
  grocery-store fixture names a DuckDB datasource with no file.)

## Where this plan is likely wrong

Three places, recorded so they can be argued with rather than
discovered.

**The job model may be over-engineering for the actual traffic.** If
every real planned statement finishes in under two seconds, `wait_ms`
answers everything and the polling endpoints are dead code with a
maintenance cost. The measurement that settles it is the distribution
of run times from phase 1's `--run`, and the cheap hedge is to build
submit-and-wait first and add the job registry when the distribution
says it is needed. The counter-argument, which is why the plan keeps
it: a cold Snowflake warehouse is tens of seconds, and discovering that
after shipping a synchronous endpoint means changing a published
contract.

**The survey's reviewer would not build this at all.** Its position is
that "never runs" should stay a near-permanent property and that the
agent's one-call experience is not worth the promise. That position is
the reason phases 0 and 1 cost nothing, phase 2 is self-hosted and
default-off, and phase 5 needs a customer. If the MCP users do not ask
for execution over the next two quarters, stopping after phase 1 is a
success, not an abandonment.

**Pre-aggregations may be a product, not a phase.** Cube built a
company on them. Phase 4 as written is conservative enough to be
small — policy-free specs only — but if it is not, the right move is to
ship phases 0 to 3 and treat cubes as a separate decision with its own
document.

## Libraries, and why each

Chosen over writing it, with the reason, since "there is a large
library of open source solutions" is true and most of them are the
wrong shape for a static binary (the survey's case against ADBC, ODBC,
ConnectorX and DataFusion-as-federator stands).

| need | library | why this one |
|---|---|---|
| columnar result, IPC, spill | `arrow`, `arrow-ipc`, `parquet` | the only lingua franca; `parquet` doubles as cube storage |
| merge engine, local SQL, file readers | DuckDB (bundled) | the `duckdb` dialect already exists; best-in-class CSV, Parquet, JSON readers; measured half DataFusion's size |
| Postgres, MySQL, SQL Server, SQLite | `tokio-postgres`, `mysql_async`, `tiberius`, `rusqlite` | pure Rust, maintained, static-musl clean; `rusqlite` is already linked |
| the eight HTTP warehouses | `hyper`/`reqwest` on `rustls` | already in the tree; eight of thirteen are JSON over HTTP with a bearer header, so the "driver" is a few hundred lines of request types each |
| Athena, Redshift Data API | `aws-sigv4`, `aws-credential-types` | the two small crates, not the SDK; the survey's measured difference |
| Arrow-native Snowflake/BigQuery/Databricks, later | `adbc_core`, `adbc_driver_manager` with `load_static` | only inside `zsql-exec`, which may be glibc-dynamic; the escape hatch if JSON transfer proves slow |
| pooling | `deadpool` | small, async, and the pool key is ours to choose, which the identity-keyed design needs |
| cancellation | `tokio-util` `CancellationToken` | one token from the HTTP future to the adapter |
| admission, timeouts, load-shed | `tower` layers over the existing limiter | already a dev-dependency; the limiter's cost classes already exist |
| sketches | `hyperloglogplus` or a Theta implementation | mergeable distinct counts, matching what the warehouses emit |
| spans and metrics | `tracing`, optional OpenTelemetry | per-job spans carrying the warehouse's query id |
| object storage, at scale-out | `object_store` | spill and cached pages readable from any instance |

Not used, and why, in one line each: ADBC's driver manager in `zsql`
(dlopen is a stub in static musl); ODBC (thirteen proprietary shared
libraries and a row-bound data path); ConnectorX (no Snowflake,
Databricks, Athena or Druid, no OAuth, pinned old `arrow`);
DataFusion (heavier than the engine it would replace, a fourteenth
dialect, buggy Substrait producer); Substrait as an interchange (the
spec moves monthly and DuckDB's extension is unmaintained — the
thirteen tested dialect files are the moat); a Python or Go executor
service (a second language and a second credential-holding boundary;
Python's right place is a client in the customer's own process, which
`docs/execution-engine.md` argues at length).

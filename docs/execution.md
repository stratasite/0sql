# Execution: the server runs what it plans

The design agreed on 2026-11-24, on branch `exec-layer`. Nothing in it
is built. `docs/exec-layer.md` is the groundwork it stands on (typed
results, ADBC, the OAuth token, the log) and `docs/exec.md` is the
earlier survey, which this replaces wherever the two disagree.

## What it is

The server gains execution. It stays one process: a run is a thread in
the server, not a second program. The earlier claim that execution
could not live in the static binary was wrong; the true constraint is
only that a static musl build cannot load an ADBC driver, so that
build runs Snowflake and BigQuery over REST.

The server sits behind an application tier. The application decides
who may ask and which credential a request carries; the database
decides what a credential may read and write. The server enforces
neither.

Two kinds of run:

| | `semantic` | `sql` |
|---|---|---|
| asked with | a spec or an expression, and a context | one SQL statement |
| the SQL | planned here, row policies compiled in | the caller's, as sent |
| cached | yes | never |
| shares an identical run in flight | yes | never: two identical `INSERT`s both run |
| retried by the server | no | no |
| may write | the planner only emits reads | yes |

## Endpoints

| | |
|---|---|
| `POST /projects/{uid}/branches/{branch}/run` | a semantic run |
| `POST /projects/{uid}/branches/{branch}/sql-exec` | one SQL statement on a named datasource, never cached |
| `GET /results/{id}` | the record, and a page of rows once it has them |
| `DELETE /results/{id}` | cancel |
| `POST /cache/invalidate` | mark results dirty by datasource and tables |

The same runs as an MCP tool come after, and as `zsql run -r` last of
all, with the rest of the command line's work.

## Credentials

To be revisited once execution exists. Until then:

| the request sends | the server's environment has | what happens |
|---|---|---|
| a secret or an OAuth token | anything | the request's is used for this run and dropped after it |
| nothing | a secret for that datasource | the server's is used |
| nothing | nothing | refused before connecting, naming the datasource and the fields it takes |
| an expired or wrong one | anything | the warehouse's refusal, as it gave it; never a fall back to the server's |

The server's variables are the ones the command line reads,
`ZSQL_DS_<UID>_<FIELD>`. The rules are the same for both kinds of run.

## Connections

- **Not read-only.** Connections are opened without a read-only
  restriction, for semantic runs too, and no statement is checked for
  being a read. What may be done to a database is the database's to
  enforce.
- **DuckDB is the exception**, for a reason that is not security: a
  writable open takes an exclusive lock on the file, which would shut
  out every other process, the command line included. A semantic run
  opens it read-only; `sql-exec` opens it writable for as long as it
  runs.
- **Pooled by credential.** A connection is kept for reuse under the
  datasource and a fingerprint of the credential it was opened with.
  With an OAuth token per user nothing is shared between users; what
  is saved is one user's burst, a dashboard's twenty queries on one
  connection. The pools are per node.

## A run

1. The record is written first (`queued`), before anything else.
2. It takes one of `ZSQL_CONCURRENT_RUNS` seats, or waits. More than
   `ZSQL_QUEUE_MAX` waiting is refused as busy.
3. The statement runs for no longer than the datasource's
   `query_timeout`, then is cancelled. The timeout covers the whole
   read, the fetch of its rows included, on every transport. That is
   not yet true of all of them: the paged REST adapters put only the
   statement under it, and MySQL has only the server's limit. Making
   it one rule is part of the second phase.
4. The request waits up to `wait` seconds, by default the datasource's
   `query_wait_time` (10 s). Ready in time, the rows are in the answer.
   Otherwise the answer is 202 and the id, and the caller asks
   `GET /results/{id}`.
5. A semantic request identical to one in flight attaches to it.

Rows go to a file a batch at a time as they arrive, so memory holds a
batch and not a result. That needs a streaming read on `Source`, which
every transport can give in principle and none gives today.

**A run can be forced.** `fresh: true` on a request skips the cache:
the statement runs even when a result that would answer is stored, and
what it returns replaces that result for everyone after. A fresh
request still attaches to an identical run already in flight, since
that one is reading the warehouse now.

**Limits are the caller's.** `ZSQL_DEFAULT_SQL_LIMIT` is unset by
default, and unset means a query with no limit of its own returns
every row. A caller that wants a bound sends one, or the operator sets
the default.

## The record

One row per run, in Postgres when `ZSQL_DB` is set (with
`ZSQL_DB_USER` and `ZSQL_DB_PASSWORD`) and kept locally otherwise. The
application creates the table; the server never does. A reference DDL
ships in this repository, and at startup the server checks the table
and its columns and says what is missing.

No ORM: `tokio-postgres`, already in the build, with `deadpool-postgres`
for the pool, every statement in one module, behind a `Registry` trait
with a Postgres and a local implementation.

| column | |
|---|---|
| `id` | unique, made by the server |
| `type` | `semantic`, `sql` |
| `status` | `queued`, `running`, `succeeded`, `failed`, `cancelled` |
| `error` | |
| `row_count` | rows returned; rows affected for a write |
| `cache_key` | empty for `sql` |
| `dirty` | set by invalidation |
| `tables` | array of physical names, as the model's `physical_name` has them; empty for `sql` |
| `org_uid` | `default` until there are orgs |
| `project_uid`, `branch`, `datasource_uid` | where it came from; `branch` is for origin only |
| `sql` | the statement run |
| `spec`, `expr` | the semantic request, whichever was sent |
| `row_limit` | the row limit in the SQL (`limit` is a reserved word) |
| `hostname` | the node running it |
| `object_store_path` | empty when the result is local only |
| `created_at`, `queued_at`, `started_at`, `finished_at`, `updated_at`, `cancelled_at`, `expires_at` | |

A credential is never in it.

**A run nobody is running.** A node refreshes `updated_at` on its runs
every 10 s. A run that is `queued` or `running` and has not been
refreshed for 60 s is marked `failed` by whichever node's cleanup sees
it, with the message that the server restarted and killed in-progress
queries. A restart keeps every record and reruns nothing.

## The cache

- **The key is the org, the project and the planned SQL.** Not the
  user, the context, the credential, the datasource or the branch: the
  row policies are already in the SQL, and the cache lives where the
  semantics do, above the datasources. Reuse is always within one org
  and project.
- **What follows from that**, stated so it is known: identical SQL
  shares a result whichever datasource or branch it ran on, so a branch
  whose datasource points at another database shares results with
  `main`; and a warehouse's own per-user row policies are not seen by
  the cache. A datasource with `cache_ttl: 0` neither reads nor writes
  the cache.
- **`cache_ttl`** on the datasource says how long a result answers.
- **Invalidation.** A semantic run records the physical tables its
  plan reads. Marking a record `dirty`, by the endpoint or by another
  system writing the column, stops it answering; the next request runs
  again.
- **Cleanup** runs every `ZSQL_CACHE_SWEEP_SECS`: expired and dirty
  results are deleted from the object store and from local disk, and
  abandoned runs are marked failed. Each node cleans its own disk.

## Where a result is kept

One interface, the `object_store` crate's, over two places.

- **With `ZSQL_OBJECT_STORE`:** a result is written to local disk as
  it arrives, uploaded under `cache/` when complete, and only then
  marked `succeeded` with its path. A node asked for a result it does
  not hold downloads it and serves later hits from disk. **If the
  upload fails the run fails**, the local file is removed, and the
  error says the result was produced and could not be stored, names
  the store and the path, and gives the store's own message: an
  operator who configured a store meant it to be used.
- **Without it:** everything is local, under the same `cache/` prefix
  in the data directory.

`cache/` leaves the rest of the store free for model backups and
whatever comes later.

**Local space.** The local result directory may take
`ZSQL_LOCAL_CACHE_MAX_PERCENT` of the disk (60). The object store has
no limit. When local space runs out: with an object store the least
recently used local copies are removed, since they are only copies;
without one the run fails with a disk-full error that recommends
configuring an object store.

**The store's credentials have their own names.** Athena falls back to
the `AWS_*` variables and BigQuery to `GOOGLE_APPLICATION_CREDENTIALS`,
so storage credentials under those names would quietly become a
warehouse login. The store's options are `ZSQL_OBJECT_STORE_<OPTION>`
(`ZSQL_OBJECT_STORE_AWS_ACCESS_KEY_ID`, and so on), handed to the store
by name and never read from the ambient variables. A machine's own
role is ambient by nature and still serves both.

## The answer

JSON unless the request's `Accept` asks for Arrow.

```json
{
  "id": "…", "status": "succeeded", "row_count": 3, "complete": true,
  "columns": [{"name": "status", "type": "string"}, {"name": "total", "type": "decimal"}],
  "rows": [["paid", "21.75"], ["open", "1"], [null, "2"]]
}
```

Values are typed: numbers, booleans, `null`, strings; a decimal as a
string, since a JSON number loses digits; dates and timestamps as ISO
strings; arrays and structs as JSON arrays and objects. JSON is paged
with `offset` and `limit`, 10,000 rows a page by default and 100,000 at
most. Arrow is the whole result as a stream.

## What an operator sets

| | where | default |
|---|---|---|
| `ZSQL_DB`, `ZSQL_DB_USER`, `ZSQL_DB_PASSWORD` | server | unset: records are local |
| `ZSQL_OBJECT_STORE`, `ZSQL_OBJECT_STORE_<OPTION>` | server | unset: results are local |
| `ZSQL_CONCURRENT_RUNS` | server | 100 |
| `ZSQL_QUEUE_MAX` | server | 5,000 |
| `ZSQL_DEFAULT_SQL_LIMIT` | server | unset: no limit |
| `ZSQL_LOCAL_CACHE_MAX_PERCENT` | server | 60 |
| `ZSQL_CACHE_SWEEP_SECS` | server | 300 |
| `ZSQL_POOL_IDLE_SECS` | server | 60 |
| `ZSQL_POOL_MAX` | server | 200, across all datasources |
| `query_timeout` | datasource | as today |
| `query_wait_time` | datasource | 10 |
| `cache_ttl` | datasource | as today; 0 is no cache |

None of these names is taken, and none can be produced by
`ZSQL_DS_<UID>_<FIELD>`.

## What reach means now

`docs/operating.md` tells an operator that whoever reaches the port
gets SQL text and the shape of the warehouse, not rows. With runs that
is rows, and with `sql-exec` and a credential held by the server it is
writes. That section is rewritten with this work; the port must not be
exposed on the old understanding.

## To verify before or while building

- **Large Databricks results.** Streaming was done for every adapter,
  and Databricks is the one whose transport cannot carry a large
  result: its inline answer has a size limit, past which it truncates
  and the read fails saying so. External links are what a large result
  needs there, and are their own phase below.
- **Go inside a long-lived server**: `docs/exec-layer.md` has the note.
- **The `object_store` crate's option names**, against its
  documentation, and reading an Arrow file from a store without
  fetching all of it.
- **Cancellation** on the transports that only time out today.
- **Which adapters restrict a connection to reads**, beyond DuckDB and
  ClickHouse, which were checked.

## What the second phase found

- **Results live under `.cache/` in the data directory**, not
  `cache/`: every other directory there is a project, and a project
  may be called `cache`. The object store's prefix is still `cache/`.
- **A failed run is not a failed request.** `run` and `results` answer
  200 with a record whose `status` is `failed`; 202 while it has not
  ended; an HTTP error only for a request that could not be taken.
- **A request with no credential is not refused before it is queued.**
  The adapter refuses when the run opens it, naming what is missing,
  and the run is recorded as failed.
- **Cancelling stops a run when its read next looks**: between
  batches, or at the timeout. A statement the warehouse is still
  computing is not interrupted there yet.
- **`tables` is found by name in the statement**, so a table whose
  name is part of another's is listed with it: more may be
  invalidated than was read, never less.
- **The cache key does what it says.** A request that sends its own
  connection for a datasource is answered from a result another
  connection made, when the project and the statement are the same.
  `fresh` asks the warehouse.

## What the fourth phase found

- **Cleanup deletes a result's rows, not its record.** In Postgres
  the row stays, with `object_store_path` emptied: the table is the
  application's, and so is how much of its history it keeps. On a
  node's own disk a record is forgotten a day after its run ended,
  once its result no longer answers, since nothing else would ever
  remove the file.
- **A result is fetched from the store when it is read**, not when it
  is found: a cache hit on another node costs nothing until its rows
  are asked for.
- **Without a store, a full disk is cleared of what no longer answers
  before a run is refused**: an invalidated or expired result does not
  hold a place.
- **The store was run against a directory**, which the same crate
  serves through the same interface. S3, GCS and Azure are that
  crate's; none was run against here.

## Not yet decided

- Credentials, after execution exists.
- Where an org uid comes from.
- The command line keeps `ZSQL_RUN_CAP` and its 10,000 rows for now;
  aligning it with `ZSQL_DEFAULT_SQL_LIMIT` joins the deferred
  command-line work.

## Order of work

1. **Streaming reads. Done, 2026-11-24.** `Source::stream` hands a
   result to a `Sink` a batch at a time, and every adapter fetches
   incrementally: DuckDB and the drivers give Arrow batches, SQLite
   steps, Postgres, MySQL and SQL Server follow the wire, the paged REST
   adapters hand over each page, and ClickHouse is read a line a row.
   `Store::begin` gives a `Writing`, a sink that puts each batch on disk
   as it comes and is a result only once committed. Nothing calls it
   from a server yet, and `zsql run` reads as it did. Run against real
   Postgres, MySQL, SQL Server, ClickHouse and Trino; the fakes for the
   rest; the driver tests by hand.
2. **A run, on one node. Done, 2026-11-24**, but for a cancel that
   reaches the warehouse (see what the phase found). What follows is
   what was asked for.
   **A run, on one node.** `query_timeout` over the whole read on
   every transport; the record and the `Registry` trait with its local
   implementation; `run`, `GET` and `DELETE /results/{id}`;
   seats and the queue; `query_timeout`, `query_wait_time`, attach;
   typed JSON, Arrow and paging.
3. **Postgres. Done, 2026-11-24.** `ops/zsql_results.sql` is the
   table; a second node joins a run in flight through its unique
   index; a result another node made is found in the table and, until
   there is an object store, answers only on the node that holds its
   rows. What was asked for:
   The registry over `ZSQL_DB`, the reference DDL, the
   startup check, the refresh and abandoned runs.
4. **The object store. Done, 2026-11-24.** What was asked for:
   Upload and download, the local percentage and
   eviction, cleanup, `tables`, `dirty` and the invalidation endpoint.
5. **Connections. Done, 2026-11-24.** A connection says how far it
   is trusted: a command still checks each statement and opens
   read-only, so `zsql` is as it was; the server's runs check nothing.
   A kept connection that fails a read before handing anything over is
   tried once more on a new one; a statement that may write never is.
   What was asked for: Writable opens, no statement check, DuckDB's two
   modes, the pool.
6. **`sql-exec`. Done, 2026-11-24.** `row_count` is the rows a
   statement changed where the transport says: SQLite, Postgres,
   MySQL, BigQuery and Trino do; DuckDB answers a write with a row
   holding the count, and the others say nothing, so it is 0 there.
   On DuckDB a `sql-exec` closes the connections kept for reading
   before it opens its own, and fails if a read is in flight on the
   same file.
7. **Around it. Done, 2026-11-24, but for the Go drivers.** The MCP
   tools are `run` and `result`, off unless `ZSQL_MCP_TOOLS` names
   them; `docs/operating.md` says what reach means now. The Go drivers
   inside a long-lived server are still untested: that needs the
   driver libraries loaded and an account, which this work could not
   do. What was asked for: The MCP tool, `docs/operating.md`, and the
   Go drivers under load.
8. **Large results from Databricks.** Its external-links mode, so a
   result past the inline limit is fetched rather than refused.
9. **The command line, last.** Everything of the command line's waits
   for this phase: `zsql run -r`, aligning `ZSQL_RUN_CAP` with
   `ZSQL_DEFAULT_SQL_LIMIT`, and the deferred list in
   `docs/exec-layer.md` (ADBC on the Linux release, fetching drivers,
   the two result shapes, the OAuth flow). No earlier phase changes
   what `zsql` does.

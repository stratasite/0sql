# `zsql run`: executing a query from the command line

A plan, written 2026-10-22 on branch `exec`. It lifts the "never
connects" ruling one step further than `docs/introspection.md` did: the
command line may run a planned query and keep its rows. The server
still never connects; nothing here is a server feature, though the
crate is shaped so a server could call it later.

Two inputs went into this. First, a design written from first
principles against this repository alone, without reference to any
other product. Second, a reading of how strata executes queries: its
`Executor::ResultSet`, the job that runs one, its cache handling and
its CLI. Where the two agree the matter is settled; where strata has
lived with a choice for longer, its experience wins; where strata's
shape is a web application's, it is left.

Four decisions belong to the product owner and are marked **[OWNER]**.

## What exists, and what this adds

Today the planner turns an expression line or a spec into SQL for one
of thirteen dialects and names the datasource; `zsql-source` opens ten
of those warehouses read-only and runs one statement under a row cap
that refuses rather than trims; `zsql sample` is the one command that
runs planned SQL, and it keeps only sampled values. There is no command
that runs a query and returns its rows, no result on disk, no cache.

This adds `zsql run`, a result store, a cache over it, and `zsql
results` to get at what was stored. One new crate owns all of it.

## The command

```
zsql run (--expr LINE | --spec FILE) [--context FILE]
         [--limit N | --all] [--fresh] [--timeout SECS]
         [--format table|json|jsonl|csv] [--out FILE] [--head N] [--quiet]
```

It reuses `QueryArgs` from `crates/cli/src/main.rs`, so a line that
works for `zsql sql` works for `zsql run`. It is local-only, like
`sample` and `introspect`: the credentials are on this machine and the
hosted service holds none, so `-r` is refused with the wording
`Where::mode` already uses.

The planner chooses the datasource, as it does for `sample`; there is
no `--datasource` flag, because that would bypass the partition and
tier ranking the model exists for.

## The lifecycle of one run

**Plan.** `Model::plan_request` as `sample.rs` calls it, giving the
SQL, `root_at`, the datasource uid and the adapter.

**Key and look up.** The cache key is computed (section "The cache")
and the store asked. A hit prints the stored rows and says so, every
time, on stderr and in the JSON output. A command that answers from
disk without saying so would be lying about the warehouse.

**The cap.** Two numbers, kept apart. The spec's `limit` is the user's
and goes into the SQL by `sql_with_limit`, as today. The engine's cap
bounds what crosses the wire: when the user gave no limit, the engine
pushes `cap + 1` into the SQL itself, reads at most that many rows,
keeps `cap`, and the extra row marks the result `complete: false`.
Default cap 10,000. `--limit N` sets both. `--all` removes the SQL
limit and replaces the row cap with a byte budget (512 MB inflated,
`ZSQL_MAX_RESULT_BYTES`), past which the run is cancelled upstream and
refused. Pushing the cap into the SQL is strata's practice too (its
`execution_sql` renders the queryable's limit into the statement): a
cap only on this side still makes the warehouse compute and ship the
whole result before it is refused, which is money spent on nothing.

Why truncate-and-flag here when `Source::query` refuses past its cap:
that rule exists for the sampler, which averages, and a partial average
is a wrong number. `run` returns rows, and ten thousand rows with a
clear "10,000 of more" footer is a better answer than a refusal. The
footer carries the honest caveat that a truncated `GROUP BY` without an
`ORDER BY` is an arbitrary subset. **[OWNER B]** truncate-and-flag
(recommended) or refuse as `sample` does.

**Open and execute.** `Connection::read` and `zsql_source::secrets`
exactly as `sample_one` does. The connect timeout is the transport's
ten seconds. The statement timeout follows strata's precedence, which
is the right one for a command that may run a long query: `--timeout`
for this run, else the datasource's `query_timeout`, else a
`query_timeout` in `project.yml`, else ten minutes. The catalogue and
sampling reads keep their sixty-second default; a run is a different
kind of statement.

**Cancellation.** Ctrl-C must reach the warehouse, not only drop the
future: a cancelled query that keeps running costs money and holds a
slot. Strata polls for a cancelled status and kills its thread without
joining it, because the thread may be blocked in a native driver; the
warehouse-side query runs on. This plan does better because the
transports already know how to cancel: `Source` gains

```rust
pub struct CancelHandle(Arc<dyn Fn() + Send + Sync>);
fn cancel_handle(&self) -> Option<CancelHandle> { None }
```

with Postgres and Redshift using the cancel token on a second
connection, Snowflake, Databricks, BigQuery and Athena the cancel
calls their timeout paths already make, Trino the `DELETE` on
`nextUri`, ClickHouse a `query_id` and `KILL QUERY`, MySQL `KILL
QUERY` from a second connection, SQL Server a dropped connection, and
SQLite its interrupt handle. The CLI installs a SIGINT handler beside
the SIGPIPE one that sets an atomic flag; the poll loops check it
between polls and the wire transports race the query against it. A
cancelled run writes nothing and exits 130.

**Memory.** Under the default cap a result is at most ten thousand rows
of text, so `Source::query` returning `Rows` whole is fine and the
first phases use it unchanged. `--all` needs a stream. Strata draws the
same line at twenty thousand rows, above which it streams the
warehouse's rows straight to a file and below which it holds them.
`Source` gains

```rust
pub trait RowSink { fn columns(&mut self, cols: &[String]) -> Result<()>; fn row(&mut self, cells: Vec<Option<String>>) -> Result<()>; }
fn query_into(&mut self, sql: &str, sink: &mut dyn RowSink) -> Result<()>  // default: query, then replay
```

The HTTP adapters already page, so their `query_into` feeds each page
to the sink; Postgres uses the raw simple-query stream; MySQL and SQL
Server their row streams. The sink is the gzip writer, so memory is one
page, and the byte budget is enforced in the sink.

**Typed cells versus text.** Cells stay text on the wire: every API
already delivers text, text is lossless, and a typed cell would make
ten transports decode ten type systems with the decimal-to-float hazard
waiting in each. What presentation needs is a type per column. Strata
does exactly this: its CSV holds text and `convert_field` applies the
projection's `output_data_type` at read time. Sources, in order of
trust: the model, through a `projections` list added to `Planned`
(alias, field uid, `data_type`); the warehouse's column metadata where
it is free (Snowflake `rowType`, BigQuery schema, Databricks manifest,
Postgres `RowDescription`); inference over the first two hundred rows
as a last resort, marked as such. The manifest records each column's
type and where it came from.

**Concurrency.** One query per invocation; several shells at once are
normal. Each result is its own directory with an id that cannot
collide, written as a temporary directory and renamed into place, as
`values.rs` writes the samples file. Strata goes one step further with
a single-flight claim: when two callers ask for the same SQL at once,
one runs it and the other subscribes to the result. That is worth
having on a command line too, because two shells (or an agent and a
human) do run the same query at once: a lock file named by the key,
holding the pid and start time, makes the second runner wait on the
first and read its result, or run itself when the lock is stale.
Second phase, not first.

## Result storage

**Where.** `~/.zsql/results/<project-uid>/<id>/`, honouring
`ZSQL_HOME` through `config::home()`, with `ZSQL_RESULTS_DIR` for a
user who wants them on an encrypted volume. Directories 0700, files
0600. Outside the project by construction, so a result cannot be
committed by a missed `.gitignore` line and cannot be deployed. Strata
keeps a local copy under `tmp/strata_cache` beside its object store;
the command line has only the local copy, and the user-level directory
is the equivalent place. **[OWNER A]** the user-level location
(recommended) over a project-local `results/`, which is friendlier for
"where is the file" and is customer data one `git add -A` from a
public repository. `--out` writes an export copy wherever the user
names.

**Format.** Parquet, written with the pure-Rust `parquet` and
`arrow-array` crates, zstd-compressed, one file per result. The first
draft of this plan said gzipped JSON lines; the second pass below says
why that changes, and what it costs. The options, measured on this
machine (twelve cores, clean release build of a scratch crate holding
only the format's dependencies; zsql's own clean build is about
seventy seconds and its stripped binary about fourteen megabytes):

| format | build | binary | typed | columnar | streams | read by other tools |
|---|---|---|---|---|---|---|
| JSON lines, gzip | 0 s, in the tree | 0 | no | no | yes | `jq`, anything |
| CSV | 0 s | 0 | no | no | yes | everything; null and empty are one |
| Parquet (arrow-rs, zstd) | 48 s | 8.5 MB in the scratch crate, less in zsql where tokio, serde and the hashers are already compiled | yes, string where unknown | yes, with per-column min and max | per row group | DuckDB, pandas, polars, Spark, every warehouse's loader |
| Arrow IPC | 40 s, the same crates less the Parquet encoder | about the same | yes | yes | yes | pandas, polars, DuckDB; not a warehouse loader's first choice |
| SQLite, a file per result | 0 s, `rusqlite` is linked | 0 | affinity only | no | yes | sqlite3; nothing in a data stack |
| DuckDB | 317 s | 41 MB | yes | yes | yes | best in class | musl C++ toolchain, already declined |

A cache file that only `zsql` can read is a cache; a Parquet file is a
dataset. Every reuse the second pass lists — a server serving slices, a
local engine, a team cache on a bucket, a corpus — reads Parquet
without conversion, and none of them reads JSON lines without one.
JSON lines costs nothing today and a migration tomorrow; Parquet costs
a minute of build today and no migration. The decision is marked
**[OWNER D]** below with the number.

The schema: each column typed by the model's `data_type` when the
model names it and every value parses as it, by the warehouse's column
type when the model is silent and the mapping is known, and `Utf8`
otherwise, with the intended type and its source kept in the file's
key-value metadata beside the manifest. A value that fails to parse as
its declared type demotes the column to `Utf8` for that result and
says so in a warning; text is never lost to a type. Rows are sorted by
the projected dimensions before writing, row groups hold about
sixty-four thousand rows, and the per-column minimum and maximum the
writer records are what let a later reader skip what it does not need.
Parquet's modular encryption is in the crate behind a feature; it is
the answer if results at rest must be encrypted, and it is not turned
on in the first phases.

CSV and JSON lines remain exports: `--format csv`, `--format jsonl`,
`--out file.csv`.

**Manifest.** `zsql_exec::Manifest`, with a `format: 1` field:

```
id, created_at, last_used_at, project_uid, branch,
datasource_uid, adapter, connection_identity (user, account, profile or project; never a secret),
spec, expr, context_digest,
sql (as sent, limit included), user_limit, cap, rows, complete, cancelled,
columns: [{name, data_type, source: model | warehouse | inferred, field_uid}],
timings: {plan_us, connect_ms, run_ms, fetch_ms}, bytes_inflated,
warehouse: {bytes_scanned, query_id} where the API reports them,
table_versions: {"<schema.table>": "<what the warehouse said>"},
lineage (lineage_format: 1): {fields: [...uids], tables: [...], partition, measures: [{uid, aggregation, additive}],
  filters: [{field, op, value, rendered, anchor}], grain: [...]},
warnings: [...]
```

A result with no rows keeps its columns, as strata's header-only CSV
does, so a table with a header and no body still says what it would
have held.

**Retention.** Age (seven days since `last_used_at`) and size per
project (500 MB, least-recently-used first), applied on every write and
by `zsql results prune`; `ZSQL_RESULTS_KEEP_DAYS` and
`ZSQL_RESULTS_MAX_MB` override. `zsql results purge` removes the
project's directory, which is the command strata's CLI offers too.

## Retrieval and presentation

**The terminal table.** Widths measured over the first two hundred
rows, cells cut at forty characters with an ellipsis, numbers
right-aligned by manifest type, nulls dimmed. On a terminal the default
shows fifty rows; piped, every row. The footer goes to stderr:
`1,234 rows (complete) · warehouse (snowflake) · 2.3 s · r-20261022-7f3a2c`
or `10,000 of more (--limit 50000 or --all) · …`, and on a hit
`· cached 4 min ago, saved 2.3 s; --fresh to run again`.

**Machine formats.** `--format json` prints the manifest, the columns
and the rows as arrays; `--format jsonl` one object per row keyed by
column name, which is what `jq` wants; `--format csv` is RFC 4180 with
empty for null. `--out FILE` picks the format from the extension. The
format never changes with whether stdout is a terminal, because scripts
that break at the first redirect are worse than one more flag.

**The `results` command.**

```
zsql results                       # id, when, rows, complete, datasource, the line or spec summary, size
zsql results show <id> [--format …] [--head N] [--offset N] [--sort COL[:desc]]
zsql results export <id> --out FILE
zsql results sql <id>              # the statement as sent
zsql results rm <id> | prune [--older 7d] | purge
zsql results stats                 # hits, warehouse seconds and bytes avoided
```

Ids accept a unique prefix. `show --offset` is strata's `paginate` over
a local file: cheap, because the file is local. `--sort` parses by
manifest type, which is why types are stored.

**Local re-query.** Worth a little, and only because the engine is
free: `zsql results query <id> "select …"` loads the file into an
in-memory SQLite table with affinity from the manifest. Honest limits
are printed. Not a merge engine, not a cube, never a cache source.
Last phase, and droppable.

## The cache

**The key.** Strata keys a result on the digest of the executed SQL
and the datasource, and nothing else, and that has held up: two specs
that plan to one statement share a result, and a model edit that
changes the SQL is a miss by construction, so nothing invalidates on
deploy. The first-principles design added a fingerprint of the whole
snapshot, which would make every model edit a miss everywhere, even an
edit to a description. Strata's rule is the better one. The key here is
`sha256` over the datasource uid, the adapter, the connection identity,
the SQL as sent (limit included) and the security-context digest. The
connection identity is in because two usernames on one warehouse may
see different rows under warehouse-side grants. The context digest is
in as a belt over the braces: a policy renders into the SQL, so the SQL
already differs, but a result planned under one principal must never
be served to another and the digest says so without reasoning about
the SQL. Secrets are never in the key.

**Staleness**, in order:

1. `--fresh`: bypass and overwrite.
2. `cache_ttl`, in seconds, on a datasource in `datasources.yml` (read
   by `Connection`, ignored by the loader like `host`) and on a table,
   the table's value winning for the tables the plan reads; `0`
   disables caching for that datasource. Strata's default is twenty-four
   hours per project; the first-principles design said one hour.
   **[OWNER C]** the default: twenty-four hours is what strata's users
   have lived with, one hour is safer for a warehouse that loads
   through the day. Either way it is one line in `datasources.yml` to
   change.
3. `data_updated_at` on a table, from the snapshot, when the result
   predates it. Nothing writes that field today; `introspect` could,
   from the warehouse's own last-modified metadata where it exists.
   Recorded as the hook.

Tier is not a freshness signal: hot, warm and cold order the planner's
preference by speed and cost, not by how often data lands, and a cold
archive is exactly the thing safe to cache longest. A partition is a
fresher copy of the same rows, and the planner already answers from it
when the filter fits; the result's SQL names the table it read, so the
cache inherits the planner's choice without a rule of its own.
Relative dates render to literals, so the same line tomorrow is
different SQL and a natural miss at day grain; the hit line says the
date the SQL was planned for.

**Near-miss.** The same question over a narrower range, served from a
superset: no. It needs a local engine that re-aggregates, and
re-aggregation is only valid for additive measures at or below the
projected grain; distinct counts, ratios and masked measures are the
trap. Not served, and said plainly.

**Cost accounting.** Each manifest carries `run_ms` and, where the API
reports it, bytes scanned. A hit appends what it saved to a per-project
`stats.jsonl`, and `zsql results stats` sums it. Money is not computed:
rates differ per account, and a wrong figure is worse than seconds and
bytes.

**Security.** A result is customer data: user-level directory, 0600,
outside the repository, never deployed, pruned by age, purgeable in one
command, keyed by principal so it is never cross-served, and said so in
`AGENTS.md` and the `results` help. `ZSQL_RESULTS=off` makes `run`
print without storing.

## A second pass: the cache as an asset

The first draft treated the store as a cache for one command. Read as
a data engineer and as someone who has run shared caches across
machines, that is the wrong frame: a cache of query results over a
semantic model is a dataset with lineage, and the cheap decisions made
now decide whether it can ever be anything else. What follows changes
the design in six places and adds the reasons.

**What the cache might be asked to be.** A server endpoint serving
slices of a stored result to many clients without re-running it; a
dashboard reading a dozen results at once; a materialised or
scheduled result refreshed on a timer, which is strata's subscription;
a team cache on a bucket, so one analyst's run serves the next; a
local engine that re-aggregates a superset result for a narrower
question, which is the cube the earlier survey set aside; a diff of
the same question across two runs, "what changed since yesterday"; and
a corpus of questions, SQL and result shapes to train the expression
model on. Each wants three things of a stored result: a format other
tools read, a record of what question it answers and over what data,
and an identity that holds across machines. The first draft had the
first wrong and the second missing.

**Identity and lineage are two different records.** The cache key is
the exact question: SQL, datasource, connection identity, context. It
must stay narrow, because a hit is a promise that the rows are the same
rows. Lineage is what the question was made of, and it is the manifest's
to carry whether or not anything reads it yet: the resolved spec with
field uids; the tables the plan read and the partition it chose; each
measure with its aggregation and whether it is additive; each filter
in canonical form, with a relative date kept as both the token
(`28d`) and the literal it rendered to and the anchor it rendered
from; the grain. None of this costs a query. All of it is impossible to
backfill. With it, a later engine can find a superset result for a
narrower question and know whether re-aggregation is even valid; a
table-level invalidation can find every result that read a table; a
corpus has its training pairs; a diff knows two results are the same
question. The schema for the lineage block is versioned on its own
(`lineage_format`) so it can grow without touching the key.

**Freshness needs a probe, not only a clock.** A TTL is a guess about
the warehouse. Most of the ten can answer "when did this table last
change" cheaply: BigQuery's `last_modified_time` and Snowflake's
`LAST_ALTERED` from `INFORMATION_SCHEMA.TABLES`, Databricks' `DESCRIBE
DETAIL`, Postgres's `pg_stat_user_tables.n_tup_ins` and friends as a
counter rather than a time, ClickHouse's `system.parts`; Athena and
SQL Server less so. The manifest records the table versions the run
saw, as `{table: version}`, in whatever form the warehouse gives. A hit
within the TTL is served as it is; `--validate` (and a datasource-level
`cache_validate: true`) re-reads the versions, one metadata query per
table, and treats a change as a miss. That is cheaper than any run and
more honest than any TTL, and it is the hook `data_updated_at` was
standing in for. The TTL stays as the bound for warehouses that cannot
answer.

**The store is a layout, not a directory.** Everything is written so a
directory can be copied to a bucket and still be a store: results are
immutable; a result is a prefix `results/<key[..2]>/<key>/` holding
`rows.parquet`, `manifest.json` and nothing else; the manifest is
written last and is the commit marker, so a reader that finds a prefix
without one skips it; the human id is an alias recorded in the index,
not the directory's name; nothing is ever updated in place, and
`last_used_at` lives in the index rather than the manifest, because a
touch on every hit is write amplification locally and a round trip on
a bucket. `Store` is a trait with a local filesystem implementation
now and room for the `object_store` crate (pure Rust; S3, GCS, Azure,
local) later; the layout does not change between them. Single flight
is a lease, not a pid: `results/<…>/<key>/lease.json` with a start
time, a holder name and an expiry a running writer renews; a second
runner waits on a live lease and takes over a stale one. A pid means
nothing on another machine; a lease with a clock does.

**An index, rebuildable from the files.** Listing three hundred
manifests is fine; listing thirty thousand, or asking "every result
that read `store_sales` in the last week", is not. A SQLite index per
store root (`index.sqlite`, through the `rusqlite` already linked)
holds the manifest fields, the lineage tables and the usage counters,
and answers `results`, `prune`, `stats` and the lineage questions. The
files remain the truth: `zsql results reindex` rebuilds it from the
manifests, and a store copied from elsewhere is indexed on first use.
This is the opposite of strata's shape, where the database row is the
truth and the file hangs off it, and it is the right way round for
something that must survive being copied.

**Types before the first row, which streaming needs.** A Parquet
writer needs the schema before the first row group, and the first
draft's inference "over the first two hundred rows" fought that. The
order is now: the model's type for every projected field (through
`Planned.projections`), the warehouse's column type for the rest
(`Rows` gains a typed column list; every transport already sees the
type and throws it away), `Utf8` for what neither names. Inference is
gone. A row group is buffered before it is written in any case, so a
declared type that a value fails is caught before the first group is
flushed and demotes the column, with the demotion in the warning list.

**What this costs and what it defers.** Parquet is a minute of clean
build and a few megabytes; the index is a schema to keep; lineage is a
block to fill from the plan, which the planner already has. Deferred,
with the measurement that defers it: the local engine. DataFusion with
Flight SQL measured 376 seconds and a hundred megabytes in the earlier
survey, DuckDB 317 seconds and forty-one with the toolchain problem.
Neither goes in now. The point of the Parquet choice is that when one
does, every result already stored is its input.

**The phases, revised.** Parquet and the lineage block go in phase 1,
because a format or a manifest field changed later is a migration of
customer data. The index and the lease go in phase 2 with `results`.
Version probing and `--validate` go in phase 3 with cancel and
timeouts, since both are per-adapter work. Streaming in phase 4 and
types in phase 5 are unchanged, except that phase 5 is smaller: the
types are decided in phase 1 and phase 5 is presentation.

## Where the work goes

A new crate `crates/exec` (`zsql-exec`), depending on `zsql-service`
for `Model`, `Planned` and `SqlRequest`, on `zsql-source`, on
`zsql-semantic` for `DataType`, and on `serde_json`, `flate2` and
`sha2`, all in the lock already. Not inside `zsql-source`: that crate
is a transport and must not know about policies, projects or disks. Not
inside the CLI: a server endpoint would reimplement it. The crate takes
a `Connector`, so the CLI supplies `datasources.yml` and `.zsql` and a
server could supply its own registry without the crate changing.

```rust
pub trait Connector { fn open(&self, datasource_uid: &str) -> zsql_source::Result<Box<dyn Source>>; }
pub struct Cancel(pub Arc<AtomicBool>);
pub enum Limit { Rows(u64), All }
pub struct RunRequest { pub query: SqlRequest, pub limit: Limit, pub fresh: bool, pub timeout: Option<Duration> }
pub enum CacheStatus { Miss, Hit { age: Duration, saved: Duration }, Bypassed, Disabled }
pub struct Outcome { pub id: ResultId, pub manifest: Manifest, pub cache: CacheStatus }
impl Engine {
    pub fn open(root: &Path, policy: CachePolicy) -> Result<Engine>;
    pub fn run(&self, model: &Model, connector: &dyn Connector, req: RunRequest, progress: &mut dyn Progress, cancel: &Cancel) -> Result<Outcome>;
    pub fn store(&self) -> &Store;
}
impl Store {
    pub fn list(&self) -> Result<Vec<Manifest>>;
    pub fn get(&self, id_or_prefix: &str) -> Result<Stored>;
    pub fn lookup(&self, key: &CacheKey) -> Result<Option<Stored>>;
    pub fn remove(&self, id: &str) -> Result<()>;
    pub fn prune(&self, keep: Retention) -> Result<Pruned>;
}
impl Stored { pub fn rows(&self) -> Result<RowReader>; }   // an iterator of rows
pub trait Progress { fn phase(&mut self, what: &str); }     // "planning", "connecting", "running", "fetching 3/7"
```

The CLI adds `run.rs`, `results.rs` and `table.rs`, and `Command::Run`
and `Command::Results` in `main.rs`. `zsql_service::Planned` gains
`projections`. `zsql_source::Source` gains `cancel_handle` and
`query_into`, each with a default so no adapter breaks.

## What strata does that is taken, and what is left

Taken: the key on SQL and datasource; the limit pushed into the SQL;
the timeout precedence of run, datasource, project, ten minutes;
streaming past a threshold and holding under it; typing at read time
from the projection's data type; a header-only result keeping its
columns; the single-flight claim, as a lock file; `purge` as a command;
the cache said to be customer data with a local copy beside the store.

Left: the result as a database row with a job queue, subscribers and
broadcasts, which is a web application's shape; the thread killed
without a join, replaced by a cancel that reaches the warehouse; CSV as
the canonical file; MD5 as the digest; the transforms tier (pivots
applied before caching), which belongs with a view layer this command
does not have.

## What to build first

Each phase ships on its own and is proven without a cloud account: the
SQLite fixture in the ordinary suite, the Postgres container on CI, and
the fakes in `crates/source/src/tests.rs`.

1. **`zsql run` and the store.** The crate with `Store`, `Manifest`,
   JSONL rows, `Engine::run` over `Source::query` under the default cap,
   `cap + 1` pushed into the SQL, `--limit`, the table printer, the
   three machine formats, `--out`. The cache with the full key, the
   default TTL, `--fresh`, the hit line. Proven: run twice on the SQLite
   fixture and the second is a hit with the same rows; change the
   context and it is a miss; a `cap + 1` row sets `complete: false`.
2. **`zsql results`** and retention: list, show, export, sql, rm, prune,
   purge, stats; the single-flight lock. Proven: three hundred small
   results pruned to the size cap, least-recently-used first; two
   concurrent runs of one query produce one result.
3. **Cancel and timeouts.** `cancel_handle` on every adapter, SIGINT in
   the CLI, `--timeout`, the project default. Proven on the Postgres
   container with `pg_sleep` (the cancel token ends it and
   `pg_stat_activity` shows it gone) and on the Snowflake fake (the
   cancel endpoint is hit, no result written).
4. **Streaming and `--all`.** `RowSink`, `query_into` for Postgres and
   the five pagers; the byte budget cancels upstream. Proven on a
   multi-partition Snowflake fake and a Databricks chunk fake.
5. **Types.** `Planned.projections`, manifest column types from model
   then warehouse then inference, right-aligned numbers and the field's
   `format`, `--sort` by type; optionally the in-memory re-query.

The cloud five remain verified by fakes until accounts exist, as the
introspection plan left them; nothing in these phases needs one.

**[OWNER D]** Parquet from the first phase, recommended, at a measured
48 seconds of clean release build and about eight megabytes in a
scratch binary (less inside zsql, which already compiles most of the
shared dependencies); or JSON lines now, at no cost, and a conversion
of every stored result the day the cache is asked to be anything else.

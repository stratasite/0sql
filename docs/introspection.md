# Introspection and sampling: building a model from the warehouse

A plan, written 2026-10-21 on branch `exec`. It narrows that branch to
the feature actually wanted now: read a warehouse's catalogue and build
a model from it, and collect sample values for its dimensions. Whether
the server later executes queries, holds cubes or caches results is a
separate decision, and `docs/exec.md` stays on this branch as the
survey of it, not as the plan of record. `docs/execution-engine.md` is
the library evaluation both rest on.

The principle that 0sql never connects to a warehouse is struck. What
replaces it is narrower and still worth saying: **0sql connects to
read, from the machine you run it on; the hosted service holds no
warehouse credentials.** That remains true through everything below.

## What this is not

Deliberately out of scope, so the phases stay small: running planned
queries, result caching, cubes, a merge engine, Arrow, DuckDB or
Polars, the job model, byte budgets and spill. Those are
`docs/exec.md`'s subject. Nothing here forecloses them; the connection
layer this builds is the one they would use.

The practical consequence is the dependency bill. This feature needs
one HTTP client and three wire-protocol crates, measured in
`docs/execution-engine.md` at about +27 s of clean build and +3 MB
stripped. It needs no columnar library at all: a catalogue is metadata
and a sample is a few hundred short strings.

## The configuration this needs already exists

Two things in the tree already anticipate this, and the work is mostly
to start reading what is already written down.

**`datasources.yml` already carries the connection.** The template
`zsql init` writes documents `host`, `port`, `database`, `username`,
`schema` and `file` under each datasource, with the comment: *"for the
tools that run the SQL on your side; 0sql never reads it."* The change
is that 0sql begins to read it. No new file, no registry, no schema
migration.

**The secret path already exists and is already enforced.** Secrets go
in `.zsql` under the same datasource key, or in
`ZSQL_DS_<KEY>_PASSWORD`; `.zsql` is gitignored and never deployed; and
`crates/cli/src/project.rs` strips twelve secret-shaped key names at
any depth from `datasources.yml` before a deploy sends it, so a nested
`oauth: {client_secret: …}` goes too. The deployed model keeps only
name, adapter and tier.

So the credential design for this phase is: implement the convention
the template already promises. `ZSQL_DS_<KEY>_PASSWORD` is documented
and not yet read by anything; make it real, and add the same shape for
tokens and key files. The elaborate connection registry in
`docs/exec.md` is for a server that executes, and is not needed here.

**The sample file already exists, and is already the right shape.**
`crates/project/src/values.rs` defines `samples/values.jsonl.gz`: one
record per dimension with its uid, name, table, `sampled_at`, sampling
mode, distinct count, the values with their row counts most frequent
first, and a min/max range for numbers and dates. Fields declare
`values: sample | full | none`. The file is gitignored, shipped in the
deploy, read by the planner, and served by `GET /values/{dim}` and the
`values` MCP tool.

Its doc comment says *"0sql never runs SQL to make it."* That single
line is what this feature changes. Everything that consumes a sample
file is built; what is missing is the thing that writes one.

## Two commands

### `zsql introspect` — the warehouse's catalogue

Connect to a datasource and read its metadata: schemas, tables,
columns, declared types, column and table comments, primary and
foreign keys, and approximate row counts. Write it to a
machine-owned file, and then generate or update model YAML from it.

**The catalogue is its own artifact, not a transient.** Introspection
writes `catalogue.json` (gitignored, never deployed) holding the
physical truth as read, with the timestamp and the server version it
came from. Model generation is a second step that reads it. Three
things follow from that separation, and they are the reason for it:

- **Regeneration stops being destructive.** The hard problem of every
  schema-to-model generator — dbt's codegen, Looker's generator,
  Cube's — is the second run, after a human has written names,
  descriptions and synonyms into the generated files. With the
  catalogue separate, generation is a diff-and-patch: new columns are
  added, dropped columns are flagged, retyped columns are flagged, and
  nothing a human wrote is overwritten. `--dry-run` prints the patch
  and is the default for a project that already has models.
- **`zsql check` gains the check it cannot do today.** The commonest
  defect in a hand-authored model is a typo in a `physical_name`, and
  nothing catches it: the loader does not know the warehouse, and the
  goldens compare text. Against a catalogue, `check` reports every
  field whose column is missing, every table whose physical name does
  not resolve, and every declared `data_type` the warehouse
  contradicts — without running a single query. This is strictly
  better than the `check --run` proposed in `docs/exec.md`, which
  needed execution to do less.
- **Drift becomes a report.** Re-introspect, diff the catalogues, and
  the answer is "three columns added, one dropped, one retyped since
  Tuesday". That is a scheduled job and a CI check, and it is the
  thing that keeps a semantic model honest over time.

**What generation infers, and how loudly.** A generated model is a
draft for a human, so each inference is annotated in the YAML with why
it was made:

- *dimension or measure* — numeric columns whose names look additive
  (amount, qty, total, price, revenue, cost) become measures with
  `sum`; numeric columns that look like keys (an `_id` suffix, an
  integer with high distinctness against row count) stay dimensions;
  dates and timestamps become dimensions with grains; everything else
  is a dimension.
- *joins* — from declared foreign keys where the warehouse has real
  ones (Postgres, MySQL, SQL Server), and from informational
  constraints where it has those (Snowflake, Redshift, BigQuery), which
  are often declared precisely so tools can read them. Where there are
  none, name matching (`orders.customer_id` → `customers.id`) proposes
  a relation and marks it unverified.
- *`cost`* — the pruner ranks candidate tables by it, so seed it from
  approximate row counts, which every warehouse exposes in statistics
  without a scan.
- *descriptions and synonyms* — copied from column and table comments.
  Most warehouses carry them and almost nobody reads them; they are the
  natural-language bridge the model needs, and they are free.

Everything inferred is marked. A human confirming a guess is cheap; a
human discovering an unmarked guess six months later is not.

### `zsql sample` — what the dimensions hold

Write `samples/values.jsonl.gz` from the warehouse, honouring each
field's declared `values:` mode and the rules the file's own
documentation already states: listable dimensions only, nothing hidden,
nothing a security policy names as a trigger.

**The output that matters most is not the values. It is the
cardinality class.** A dimension with twelve distinct values (status,
region, channel) should have all twelve listed. One with ten million
(customer id, email, order number) should have none listed, and should
instead carry its distinct count and a shape — "32 hex characters",
"an email address", "an integer between 1 and 8.4 million". The class
is what tells a consumer whether to match a phrase against values at
all, and it is the difference between a useful sample file and a
privacy incident with no upside.

So sampling is two passes per table:

1. **One scan per table**, not one per column: approximate distinct
   counts for every sampled column at once, plus min and max for
   numbers and dates. Every target has an approximate distinct function
   (`APPROX_COUNT_DISTINCT`, `HLL`, `uniqCombined`, `COUNT(DISTINCT)`
   on the small ones), and one query over all columns costs one scan
   instead of N.
2. **Top-N by frequency** for the columns the first pass classified as
   low-cardinality, batched where the dialect allows, with `TABLESAMPLE`
   or its equivalent on large tables. Most frequent first is what the
   file format already stores, and it is more useful than random
   distinct values for every consumer.

**Cost has to be a first-class argument here**, because this is the one
place in this feature that can spend real money. A model with 353
fields naively sampled is several hundred warehouse queries, which on
a per-byte warehouse is a bill the user did not expect from a command
that sounds like metadata. So: a budget in queries and in bytes, a
printed estimate before anything runs, `--dry-run` showing the plan, a
concurrency cap, and a statement timeout on every query. The
`query_timeout` already on `Datasource` is the default.

## Governance: two different questions about the same file

The existing design is careful about samples *inside a customer's own
project*: gitignored, deployed with the model, restricted to listable
dimensions, excluding anything a policy triggers on. That is the right
posture for a file that only ever travels from a customer's warehouse
to that customer's own deployed model.

The stated purpose here is different, and the difference is the whole
of the risk: **these samples are intended to train a model that 0sql
will deploy.** That is a second act, with a second set of permissions,
and it does not inherit the first one's consent. A customer who
accepted that their product names ride along inside their own model has
not thereby agreed that those names enter a corpus.

What this plan holds, and would want argued with rather than assumed:

- **Collection and corpus are separate decisions, separately
  consented.** `zsql sample` writes the customer's file. Anything that
  pools files into a training corpus is a different command, a
  different opt-in, and ideally a different document.
- **Classification before collection, not after.** Deny by column name
  pattern (email, phone, ssn, dob, address, name, account) and by
  detected format in the sampled values, defaulting to `values: none`
  for anything that trips either, and say so in the output. A field
  wrongly excluded costs a flag; a field wrongly included cannot be
  un-collected.
- **The free-text column is the sharp edge.** A low-cardinality status
  column is safe; a `notes` or `comment` column can contain anything a
  human typed, including the identifiers every other rule excludes.
  Default free text out, by type and by average length, not by name.
- **Say where it goes.** Whatever is decided, the sample file's own
  documentation and the command's output should state it plainly,
  because that comment is the thing a customer's security reviewer will
  read.

This is the one part of this plan that should be settled before code,
since it governs what the code is allowed to collect.

## What a model that writes expressions needs beyond this

Sample values ground entities: knowing that `region` contains
"California" and `state_code` contains "CA" is what lets a request name
one and get the other. They are necessary and they are not sufficient.
What else the repository already holds, and should be gathered
deliberately rather than rediscovered later:

- **The goldens** are thousands of (spec, SQL, universe) triples with
  their security contexts, in `fixtures/golden`. They are the largest
  existing corpus of correct 0sql, and they are never edited, which
  makes them a stable reference.
- **The refusals are better signal than the successes.** The planner
  refuses an unknown or ambiguous field and names the nearest ones with
  their uids, kinds and tables. A corpus of (phrase → refusal →
  corrected expression) teaches disambiguation, which is the thing a
  natural-language front end actually gets wrong.
- **`corrections`** — every near-match resolution the planner already
  reports is a labelled (what they wrote, what it meant) pair, produced
  by real use.
- **`harness/mcp-eval`** already runs a real client against the demo
  model and grades the spec it settles on by the rows returned. It is
  the evaluation harness for this, not just for MCP tool descriptions.
- **The expression language has a formal grammar**
  (`docs/expression-language.md`, PEG, implemented in
  `crates/shorthand`). A grammar means generated candidates can be
  checked for validity before they are scored, and invalid output can
  be made unrepresentable rather than merely penalised.

None of that is this plan's work. It is listed because the sampling
format should not be designed as though values were the only thing the
model will be given.

## The adapter surface, which is narrower than execution's

The reason this phase is far cheaper than `docs/exec.md`: executing
needs thirteen result protocols, but introspection mostly needs one
query. `INFORMATION_SCHEMA.TABLES` and `.COLUMNS` are ANSI, and
Postgres, MySQL, SQL Server, Snowflake, BigQuery, Databricks, Trino and
DuckDB all implement them, so one implementation with small per-adapter
variations covers most of the list. The exceptions are known and small:
ClickHouse's `system.tables`, Redshift's `SVV_*` views, SQLite's
`PRAGMA`, Druid's `INFORMATION_SCHEMA` subset, and Athena, whose
catalogue is Glue rather than SQL.

Sampling is `SELECT`, `GROUP BY`, `COUNT`, `MIN`/`MAX` and an
approximate-distinct function — which is to say, it is the dialect
layer that already exists, pointed at a small fixed set of statements.

## Phases

- [ ] **A — connect and read.** The connection from `datasources.yml`
      plus `.zsql` and `ZSQL_DS_<KEY>_*`; a `Catalogue` type;
      `zsql introspect` writing `catalogue.json` for two adapters
      (DuckDB and Postgres), with a timeout, a row cap and no writes to
      the warehouse ever. *Gate:* binary growth and clean build time
      measured against `docs/execution-engine.md`'s table.
- [ ] **B — model from catalogue.** Generation with marked inferences;
      diff-and-patch on re-run with `--dry-run` as the default for an
      existing project; `zsql check` validating a model against a
      catalogue; a drift report between two catalogues.
- [ ] **C — sampling.** The two-pass design, the cardinality classes,
      the budget and the estimate, and the governance rules above
      written into the command and the file's documentation. Settle the
      corpus question before this phase, not during it.
- [ ] **D — the rest of the adapters**, ordered by who asks.
- [ ] **E — the corpus**, as its own decision and its own document,
      drawing on the goldens, the refusals and the corrections as much
      as on the samples.

## To verify before building

- Which of the thirteen expose column and table comments through
  `INFORMATION_SCHEMA`, and which need a vendor query for them. The
  comments are the highest-value field in the catalogue and the least
  uniform.
- Whether informational foreign keys are declared often enough in
  Snowflake, BigQuery and Redshift to be worth reading, or whether name
  matching carries the join inference in practice.
- The cheapest approximate-distinct per adapter, and whether one query
  over all of a wide table's columns is actually cheaper there than one
  per column — it is on a columnar warehouse, and may not be on a row
  store.
- What a realistic sample run costs on a real warehouse, in queries,
  bytes and money, for a model the size of the TPC-DS fixture. This
  number decides whether the budget is a safeguard or a constant
  companion.
- Whether `catalogue.json` should be gitignored like the samples, or
  committed so that drift is visible in a pull request. There is a good
  argument for committed: it is not customer data, it is schema, and a
  diff in review is exactly where drift wants to be seen.

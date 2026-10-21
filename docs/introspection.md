# Introspection and sampling: building a model from the warehouse

A plan, written 2026-10-21 on branch `exec`. It narrows that branch to
the feature wanted now: read a warehouse's catalogue and build a model
from it, and collect sample values for its dimensions. Whether the
server later executes planned queries, holds cubes or caches results is
a separate decision; `docs/exec.md` stays on this branch as the survey
of that, not as the plan of record, and `docs/execution-engine.md` is
the library evaluation both rest on.

The principle that 0sql never connects to a warehouse is struck. What
replaces it is narrower and still worth saying: **0sql connects to
read, from the machine you run it on; the hosted service holds no
warehouse credentials.** That stays true through everything below.

## What this is not

Deliberately out of scope so the phases stay small: running planned
queries for a caller, result caching, cubes, a merge engine, Arrow,
DuckDB or Polars, the job model, byte budgets and spill. Also out, for
now, by decision: value ranges, and any inference of a column's format
or pattern.

The practical consequence is the dependency bill. This needs one HTTP
client and three wire-protocol crates — about +27 s of clean build and
+3 MB stripped, measured in `docs/execution-engine.md` — and no
columnar library at all. A catalogue is metadata; a sample is a few
hundred short strings.

## The configuration this needs already exists

**`datasources.yml` already carries the connection.** The template
`zsql init` writes documents `host`, `port`, `database`, `username`,
`schema` and `file` under each datasource, with the comment *"for the
tools that run the SQL on your side; 0sql never reads it."* The change
is that 0sql begins to read it. No new file, no registry, no migration.

**The secret path already exists and is already enforced.** Secrets go
in `.zsql` under the same datasource key, or in
`ZSQL_DS_<KEY>_PASSWORD`; `.zsql` is gitignored and never deployed; and
`crates/cli/src/project.rs` strips twelve secret-shaped key names at
any depth from `datasources.yml` before a deploy sends it, so a nested
`oauth: {client_secret: …}` goes too. The deployed model keeps only
name, adapter and tier. `ZSQL_DS_<KEY>_PASSWORD` is documented and read
by nothing yet: making it real, and adding the same shape for tokens
and key files, is this plan's credential design in full.

**The sample file already exists, with all its consumers.**
`crates/project/src/values.rs` defines `samples/values.jsonl.gz`, read
by the planner and served by `GET /values/{dim}` and the `values` MCP
tool. Its doc comment says *"0sql never runs SQL to make it."* That
line is what this feature changes: everything that consumes a sample
file is built, and nothing writes one. Because nothing writes one, the
record format is free to change without migrating anything.

## The model changes

Four keys on a dimension change. Names first, then the parts that are
not just names.

| today | becomes | notes |
|---|---|---|
| `disable_listing` (alias `disable_value_listing`) | `disable_sampling` | the key never governed listing; it governs whether a value is collected |
| `value_list_size`, default 1000 | `sample_size`, default 1000 | the limit the sampling query applies |
| `values: sample \| full \| none` | *removed* | full versus sample is now inferred from what was collected, not declared |
| — | `sample_sql` | an optional query the author writes, used instead of the generated one |

**Sampling is on by default only for `string`.** `DataType` has eight
variants — `String`, `Integer`, `Decimal`, `Date`, `DateTime`,
`Boolean`, `Bigint`, `Binary` — and there is no separate text type, so
"string and text" is `DataType::String` alone. Every other type is off
by default: a sampled integer is usually a key, a sampled date is a
range question this plan has dropped, and a sampled boolean is
worthless. `data_type` is `Option<DataType>`, and **an absent type is
treated as off**, because the alternative is guessing from
`display_type` and collecting customer data on a guess. An author who
wants a non-string dimension sampled says so by giving it a
`sample_sql`, which is an explicit act.

Two existing flags stay as exclusions, and are named here so they are
not forgotten when `values:` goes away: a `secure` field is not
sampled, and neither is a field a security policy names as a trigger,
by tag or by name. The sample file's own documentation already said the
second one; nothing enforced it, because nothing wrote the file.

**`sample_size` and the total, so completeness is inferred.** Each
record carries the `sample_size` the query ran with and the column's
total distinct count. A sample is complete when the total is known and
no greater than `sample_size`. Recording both, rather than a declared
mode, means the file says what happened instead of what was asked for.

Getting an exact total costs a `COUNT(DISTINCT …)`, so it is batched:
one query per table computing the distinct count of every sampled
column at once, which is one scan rather than one per column. Where a
total is refused by budget, the record carries the sample with the
total absent, and completeness reads as unknown rather than as
complete. The cheap alternative — ask for `sample_size + 1` rows and
infer truncation from getting them — is noted because it costs nothing,
but it answers "is it complete" without answering "how many are there",
and the second answer is the one a consumer wants.

**The generated sampling query.** When a dimension has no
`sample_sql`, 0sql writes it with the SQL layer it already has —
`crates/planner/src/sql/expr.rs` to render the field's expression and
`crates/planner/src/sql/builder.rs` to assemble the statement — so the
dialect, the quoting and the expression are the planner's, not a
`format!`. A dimension may be an expression rather than a bare column
(`expression: {sql: …}`, a lookup, an array), which is exactly why the
rendering has to go through that layer:

```sql
SELECT <rendered expression> AS v, COUNT(*) AS n
FROM <physical table>
[WHERE <the table's partition predicate>]
GROUP BY <rendered expression>
ORDER BY n DESC
LIMIT <sample_size>
```

Most frequent first, which is what the record format already stores and
what every consumer wants. Two things about it that are easy to miss:

- **The partition predicate is not optional.** `Partition` on a table
  carries a dimension, a predicate and filter values, and a `GROUP BY`
  over a partitioned fact table with no partition filter is a full
  scan of the whole history. The model already holds what is needed to
  avoid that, so sampling applies it, and a table whose partition
  cannot be satisfied is skipped with a reason rather than scanned.
- **This is the first feature that runs generated SQL, so the row
  limit has to be dialect-correct first.** `Plan::sql_with_limit`,
  `crates/planner/src/dag.rs` and `crates/planner/src/sql/builder.rs`
  each append `"LIMIT n"` with no dialect indirection, and
  `dialects/sqlserver.yml` has no limit, offset or fetch template
  because SQL Server has no `LIMIT`. Nothing catches it today: the
  goldens compare text and nothing executes. A dialect-rendered limit
  is a prerequisite for sampling, not a follow-up, and it is its own
  commit.

**`sample_sql`, and what it is allowed to be.** The author's query is
used as given, in their dialect, against their warehouse, from their
machine. Its contract: it returns the value in the first column and,
optionally, a count in a second; it applies its own limit, and 0sql
records the `sample_size` it was configured with so the record stays
readable. Raw SQL in a model is not a new idea here —
`expression: {sql: …}` is already a field key — but `sample_sql` is
different in one way that matters: it is *executed*, and it travels in
the deploy archive. So if a server-side sampling path is ever built, a
deployed `sample_sql` must not run with server credentials. It is
stripped from the archive, or refused there; decided now, while there
is no such path to forget about.

### The rename has two mechanical consequences

**The goldens carry the old names, and the goldens are never edited.**
`fixtures/golden/universes/*.json.gz` hold serialized snapshots whose
fields include `disable_listing` and `value_list_size` —
`value_list_size` appears 24,034 times — and `zsql_semantic::Field` is
`#[serde(deny_unknown_fields)]`. So renaming the Rust fields without
`#[serde(alias = "value_list_size")]` and
`#[serde(alias = "disable_listing")]` breaks the deserialization of
every golden universe and takes the whole scoreboard with it. The
aliases are mandatory, not a courtesy.

The same courtesy is owed to projects in the wild, where the YAML keys
are the ones people typed: the loader keeps `disable_listing`,
`disable_value_listing` and `value_list_size` in `FIELD_KEYS` as
accepted spellings that load with a warning, and `values:` likewise —
accepted, ignored, warned — rather than becoming an unknown-key
refusal on someone's existing model.

**One literal needs renaming in lockstep.** `crates/planner/src/
pruner.rs` emits `"value_list_size": 0` inside a synthetic field it
builds as JSON. It is the only place outside the loader and the
snapshot that names the key.

**And a note on what these keys do today: nothing.** Neither
`disable_listing` nor `value_list_size` is read anywhere.
`Model::values_of` serves whatever the sample file holds and consults
neither. So the rename is free of behaviour change, and it is worth
being clear that `disable_sampling` governs *collection* only. If
controlling *exposure* of already-collected values is ever wanted,
that is a different key, and conflating the two would be the easy
mistake.

### What the authoring surface has to say

The change is not done when the code compiles. Three places speak to
people and to agents, and all three name the old keys:

- `crates/cli/src/templates.rs` — the commented field block that
  `zsql init` writes shows `values: sample  # sample | full | none`.
  It becomes `sample_size`, `disable_sampling` and `sample_sql`, with
  the string-only default stated in the comment, since this template
  is where most authors learn the key exists.
- `crates/server/src/mcp_prompts.json` — the `values` tool description
  says values come back "when the project uploaded samples". Uploading
  is no longer the only way they arrive, and an agent should be told
  that a dimension may have no sample because its type is not string,
  which is a different answer from "this project has none".
- The published docs behind `0sql.io/docs`, which are outside this
  repository, and the model reference in them.

## Two commands

### `zsql introspect` — the warehouse's catalogue

Connect to a datasource and read schemas, tables, columns, declared
types, column and table comments, primary and foreign keys, and
approximate row counts. Write it to `catalogue.json`, **committed, not
gitignored**: it is schema, not customer data, and a diff in review is
exactly where schema drift wants to be seen.

Model generation is a second step that reads the catalogue. Three
things follow from that separation, and they are the reason for it:

- **Regeneration stops being destructive.** The hard problem of every
  schema-to-model generator is the second run, after a human has
  written names, descriptions and synonyms into the generated files.
  With the catalogue separate, generation is a diff-and-patch: new
  columns added, dropped and retyped columns flagged, nothing a human
  wrote overwritten. `--dry-run` prints the patch and is the default
  for a project that already has models.
- **`zsql check` gains the check it cannot do today.** The commonest
  defect in a hand-authored model is a typo in a `physical_name`, and
  nothing catches it: the loader does not know the warehouse and the
  goldens compare text. Against a catalogue, `check` reports every
  field whose column is missing, every table whose physical name does
  not resolve, and every declared `data_type` the warehouse
  contradicts — without running a query.
- **Drift becomes a report**, and with the catalogue committed, a pull
  request.

**What generation infers, and how loudly.** A generated model is a
draft for a human, so every inference is annotated in the YAML with
why it was made:

- *dimension or measure* — numeric columns whose names look additive
  (amount, qty, total, price, revenue, cost) become measures with
  `sum`; numeric columns that look like keys (an `_id` suffix, high
  distinctness against row count) stay dimensions; dates and
  timestamps become dimensions with grains; everything else is a
  dimension.
- *joins* — from declared foreign keys where the warehouse has real
  ones (Postgres, MySQL, SQL Server) and from informational
  constraints where it has those (Snowflake, Redshift, BigQuery),
  which are often declared precisely so that tools can read them.
  Failing both, name matching (`orders.customer_id` → `customers.id`)
  proposes a relation and marks it unverified.
- *`cost`* — the pruner ranks candidate tables by it, so seed it from
  approximate row counts, which every target exposes in statistics
  without a scan.
- *descriptions and synonyms* — copied from column and table comments.
  Most warehouses carry them, almost nobody reads them, and they are
  the natural-language bridge a model needs.

### `zsql sample` — what the dimensions hold

Write `samples/values.jsonl.gz` from the warehouse, over the
dimensions that `disable_sampling`, the type default, `secure` and the
policy triggers leave eligible, each with its own `sample_size` and its
own or the generated query.

Whether the file is committed is **the author's choice**, not the
tool's: it is deployed either way, and it is customer data, so the
scaffold keeps it in `.gitignore` and says in a comment that removing
the line is a decision about where those values are allowed to go.

**Cost is a first-class argument here**, because this is the one place
in this feature that spends real money. A model with 353 fields,
sampled naively, is several hundred warehouse queries from a command
that sounds like metadata. So: the distinct counts batched one query
per table; a budget in queries; a printed plan and estimate before
anything runs; `--dry-run`; a concurrency cap; and a statement timeout
on every query, defaulting to the `query_timeout` already on
`Datasource`. `--only <table-or-field>` so re-sampling one dimension
does not re-sample the model.

## Governance: two different questions about the same file

The existing design is careful about samples *inside a customer's own
project*: gitignored by default, deployed with the model, excluding
hidden fields and policy triggers. That is right for a file that
travels from a customer's warehouse to that customer's own model.

The stated purpose here is different, and the difference is the risk:
**these samples are meant to train a model that 0sql will deploy.**
That is a second act with a second set of permissions, and it does not
inherit the first one's consent. A customer who accepted that their
product names ride along inside their own model has not thereby agreed
that those names enter a corpus.

So: collection and corpus are separate decisions, separately consented.
`zsql sample` writes the customer's file and nothing else. Anything
that pools files into a training corpus is a different command, a
different opt-in, and its own document. The string-only default helps
less than it looks — names, emails and addresses are all strings — so
the deny rules still matter: by column name (email, phone, ssn, dob,
address, name, account) and by the `secure` flag and policy tags
already in the model, defaulting those to not sampled and saying so in
the output. A field wrongly excluded costs a flag; a field wrongly
included cannot be un-collected.

The sharp edge is free text. A low-cardinality status column is safe; a
`notes` or `comment` column holds whatever a human typed, including the
identifiers every other rule excludes, and it is a `string` like any
other. With pattern detection out of scope, the available signal is
average value length and distinct count against row count, and the
honest answer is that those are weak. A `notes` column that trips
neither rule will be sampled. Either the deny list carries the weight,
or sampling is opt-in per field rather than on by default for strings —
which is the one place this plan would most readily be argued out of
its current position.

This part should be settled before phase C, since it governs what the
code is allowed to collect.

## What a model that writes expressions needs beyond this

Sample values ground entities: knowing that `region` holds "California"
and `state_code` holds "CA" is what lets a request name one and get the
other. Necessary, not sufficient. What the repository already holds,
and should be gathered deliberately rather than rediscovered:

- **The goldens** — thousands of (spec, SQL, universe) triples with
  their security contexts in `fixtures/golden`, never edited, so a
  stable reference.
- **The refusals, which are better signal than the successes.** The
  planner refuses an unknown or ambiguous field and names the nearest
  ones with their uids, kinds and tables. A corpus of (phrase →
  refusal → corrected expression) teaches disambiguation, which is
  what a natural-language front end actually gets wrong.
- **`corrections`** — every near-match the planner reports is a
  labelled (what they wrote, what it meant) pair from real use.
- **`harness/mcp-eval`** — already runs a real client against the demo
  model and grades the spec it settles on by the rows returned.
- **The expression grammar** (`docs/expression-language.md`, PEG,
  implemented in `crates/shorthand`), which means candidates can be
  checked for validity before they are scored.

## The adapter surface, which is narrower than execution's

Executing needs thirteen result protocols; introspecting mostly needs
one query. `INFORMATION_SCHEMA.TABLES` and `.COLUMNS` are ANSI, and
Postgres, MySQL, SQL Server, Snowflake, BigQuery, Databricks, Trino and
DuckDB all implement them, so one implementation with small
per-adapter variations covers most of the list. The exceptions are
known and small: ClickHouse's `system.tables`, Redshift's `SVV_*`
views, SQLite's `PRAGMA`, Druid's `INFORMATION_SCHEMA` subset, and
Athena, whose catalogue is Glue rather than SQL.

Sampling is `SELECT`, `GROUP BY`, `COUNT`, `ORDER BY` and a limit —
the dialect layer that already exists, pointed at two fixed statements.

## Phases

- [ ] **A — the keys.** The dialect-rendered row limit and its goldens,
      first and on its own. Then `disable_sampling`, `sample_size`,
      `sample_sql`, the removal of `values:`, the string-only default,
      the serde aliases the goldens need, the `pruner.rs` literal, the
      loader's accepted-with-a-warning spellings, and the three
      authoring surfaces. No connection yet, so this phase is pure
      refactor and ships behind nothing. *Gate:* the golden scoreboard
      unchanged, and a test that loads a project using every old
      spelling and warns on each.
- [ ] **B — connect and read.** The connection from `datasources.yml`,
      `.zsql` and `ZSQL_DS_<KEY>_*`; a `Catalogue` type;
      `zsql introspect` writing `catalogue.json` for two adapters
      (DuckDB and Postgres), with a timeout, a row cap, and no
      statement that is not a read. *Gate:* binary growth and clean
      build time measured against `docs/execution-engine.md`'s table.
- [ ] **C — model from catalogue.** Generation with marked inferences;
      diff-and-patch on re-run with `--dry-run` the default for an
      existing project; `zsql check` against a catalogue; a drift
      report between two catalogues.
- [ ] **D — sampling.** The generated query through the planner's SQL
      layer, partition predicates applied, batched distinct counts, the
      budget and the estimate, `sample_sql`, and the governance rules
      above written into the command, the template and the file's
      documentation.
- [ ] **E — the rest of the adapters**, ordered by who asks.
- [ ] **F — the corpus**, as its own decision and its own document,
      drawing on the goldens, the refusals and the corrections as much
      as on the samples.

## To verify before building

- Which of the thirteen expose column and table comments through
  `INFORMATION_SCHEMA` and which need a vendor query. The comments are
  the highest-value field in the catalogue and the least uniform.
- Whether informational foreign keys are declared often enough in
  Snowflake, BigQuery and Redshift to be worth reading, or whether
  name matching carries the join inference in practice.
- What a sample run actually costs on a real warehouse, in queries,
  bytes and money, for a model the size of the TPC-DS fixture. This
  decides whether the budget is a safeguard or a constant companion.
- Whether a batched `COUNT(DISTINCT …)` over every sampled column of a
  wide table beats one query per column. It should on a columnar
  warehouse and may not on a row store.
- Whether any project in the wild sets `values: full`, since that
  declaration disappears and those authors get the inferred answer
  instead.

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

Three existing properties stay as exclusions, and are named here so
they are not forgotten when `values:` goes away: a `hidden` field, a
`secure` field, and a field a security policy names as a trigger, by
tag or by name. `crates/project/src/values.rs:8` already named all
three; nothing enforced any of them, because nothing wrote the file.

`hidden` is the one most easily missed, and it is not redundant:
`Model::values_of` serves hidden dimensions deliberately
(`crates/service/src/lib.rs:526`, "hidden ones included"), so excluding
them at collection is the only place it happens.

And a gap worth stating rather than discovering: **`values_of` applies
no security policy.** A sampled field that a `mask_data` policy would
mask is served verbatim to any caller with read access on the branch.
Excluding policy *trigger* fields at collection does not cover a field
whose values a policy filters or masks for some callers. Closing that
means evaluating policies on the values read path, which is not this
plan's work, but collection should not make it worse — so a field named
by any policy, in any role, is excluded until that path exists.

**`sample_size`, and completeness inferred from what is already
there.** The record format already carries most of this:
`DimensionValues.distinct` is the column's total distinct count, and
`values.len()` is how many came back — `crates/cli/src/show.rs:196`
already prints "N of M values" from exactly those two. So the only new
field is `sample_size`, the limit the query ran with, and it earns its
place in one case: when the total was refused by budget, `distinct` is
absent and `values.len() == sample_size` is the only thing that says
the sample is truncated rather than complete.

A sample is complete when the total is known and no greater than
`sample_size`; truncated when the list is as long as the limit; and of
unknown completeness otherwise. The file says what happened instead of
what was asked for, which is the point of dropping the declared mode.

Getting an exact total costs a `COUNT(DISTINCT …)`, so it is batched:
one query per table over every sampled column at once, which is one
scan rather than one per column. The cheap alternative — ask for
`sample_size + 1` rows and infer truncation from getting them — costs
nothing but answers "is it complete" without answering "how many are
there", and the second answer is the one a consumer wants.

**`mode` leaves the record and `range` stays unwritten.** `mode` had
no source once `values:` is gone, so it comes out of
`DimensionValues`; the struct is not `deny_unknown_fields`, so a sample
file written by the old external tooling still loads with its `mode`
ignored. `range` stays in the struct and is simply never written,
because `show.rs:183` branches on it and an existing file that has one
should keep rendering. That keeps the change to `GET /values/{dim}` and
the `values` MCP tool additive, which matters more than tidiness:
`crates/server/src/parity_tests.rs:239` asserts that response equals
`Model::values_of` over more than forty specs, so the record is a
tested public contract, not an internal format.

**`sample_size` needs a ceiling, because the failure mode is total.**
`ValueCatalog::write` enforces no size limit, `read` refuses past
`INFLATED_LIMIT` (256 MB inflated), and
`crates/project/src/lib.rs:1616` turns that refusal into a *warning*
with `values: None` — so one dimension with an enormous
`sample_size` silently costs every other dimension its values. The cap
belongs at write time, as a refusal naming the dimension, not at read
time as a warning about the file.

**The generated sampling query.** When a dimension has no
`sample_sql`, 0sql writes the statement for it:

```sql
SELECT <rendered expression> AS v, COUNT(*) AS n
FROM <physical table>
[WHERE <the table's partition predicate>]
GROUP BY <rendered expression>
ORDER BY n DESC
LIMIT <sample_size>
```

Most frequent first, which is what the record format already stores
and what every consumer wants. Four things about it are harder than
they look.

**There is no reusable expression renderer, and the first draft of this
plan was wrong to claim one.** `crates/planner/src/sql/expr.rs` is
`qualify`, `column_gsub` and `entity_gsub` — string rewrites over an
SQL fragment that is already known. The real renderer is
`expression_sql` in `crates/planner/src/visitors/sql.rs:651`, it is
private, and its `Ctx` holds a `&Graph`, a `&Universe` and an `&RQuery`
— a planned query graph, which a sampling command does not have.
`sql::builder::Builder` carries no dialect at all; `to_sql` is
dialect-free assembly. So what sampling can actually reuse is
`expr::qualify` plus `Dialect::quote`, which covers a dimension that is
a column or a plain `expression: {sql: …}` over its own table. That is
the honest scope of phase D.

**Which means calculated, lookup and array dimensions are out of
scope.** A calculated dimension resolves through
`ref_to_sql_on_node` (`visitors/sql.rs:657`), which needs sibling nodes
that a one-table sampling query does not have. An `array` expression
needs `dialect.array_unnest_join` via the private `unnest_array`
(`visitors/sql.rs:243`), and `GROUP BY` over an array or variant column
is invalid on BigQuery and Snowflake anyway. These dimensions are
skipped with a reason, and `sample_sql` is how an author samples one.
That is a large part of what `sample_sql` is for, and a better
justification for it than "a custom query".

**Partition filters cannot be taken from the model, and the first
draft was wrong about this too.** The planner never emits a partition
predicate: `partition_score` in `crates/planner/src/pruner.rs:101` only
*ranks* candidate tables by whether the caller's own filters fall
inside a partition, and `within_partition` is a comparison, not a
renderer. Worse, `Partition.filter_value` is an authoring default
rather than the current partition — across the golden universes the
partitions are all `between`, with values like `1980-01-01` to
`1995-01-01`, and `crates/cli/src/templates.rs` documents relative
tokens such as `2y` that need `sql/dates.rs` to become dates.
Rendering those verbatim would sample a stale historical window and
silently bias every sampled dimension on a partitioned table, which is
worse than not sampling it. So: a partitioned table is sampled only
with an explicit window — `--since`, or the author's own `sample_sql` —
and is otherwise skipped with a reason. "Skip when the partition cannot
be satisfied" was no protection, because every partition in every
fixture has a value.

**`COUNT(*)` counts the rows of the table the dimension lives on.** On
a dimension table that is dimension rows, so a near-unique column
returns `n = 1` for every value and "most frequent first" degenerates
to arbitrary order — which `show.rs:196` and
`crates/cli/src/assist.rs:54` both present as meaningful. Weighting by
the fact table needs a join this template cannot express. The
frequency is therefore documented as "rows of this dimension's own
table", and a fact-weighted count is a later question.

**`sample_sql`, and what it is allowed to be.** The author's query is
used as given, in their dialect, against their warehouse, from their
machine. Its contract: the value is the first column and an optional
count is the second; it applies its own limit, and 0sql records the
`sample_size` it was configured with so the record stays readable.

It is not stripped from the deploy archive, and it should not be.
`crates/cli/src/project.rs:120` adds every `models/**/*.yml`
byte-for-byte; only `datasources.yml` is parsed, stripped and
re-serialized. Stripping a key inside a model file would mean parsing
and re-emitting all of them, which destroys comments and key order in
files that `zsql init` writes as mostly comments. So the rule is the
other way round: **a deployed `sample_sql` is never executed.** There
is no server-side sampling path today, and if one is ever built it
ignores `sample_sql` entirely and samples only what it can generate
itself. That is a one-line rule in the right place instead of an
archive rewrite, and it is decided here rather than left open.

### The rename has four mechanical consequences

**The snapshot format number must be raised to 2.**
`crates/semantic/src/snapshot.rs:13` holds `FORMAT: u32 = 1`, and its
own doc comment says to raise it "in the same change that makes a
snapshot a reader of the previous number could not understand: a field
added without a serde default, or one renamed or removed, since every
struct here is `deny_unknown_fields`." This change renames one field
and removes another, which is both cases at once. Aliases fix only the
read direction; the new build *writes* `sample_size`, and
`crates/service/src/lib.rs` writes a real `snapshot.json.gz` per
deployed branch. Without the bump, a rollback to the previous server —
which `docs/releasing.md` and the last two releases treat as the
standard recovery — reads a snapshot it cannot parse and reports a raw
serde error instead of the "deploy it again from its project" message
that the `Newer` branch exists to give. `FORMAT = 2` is part of phase
A, not an afterthought.

**The goldens carry the old names, and the goldens are never edited.**
`fixtures/golden/universes/*.json.gz` hold serialized snapshots whose
fields include `disable_listing` and `value_list_size` —
`value_list_size` appears 24,034 times — and `zsql_semantic::Field` is
`#[serde(deny_unknown_fields)]` with `value_list_size` carrying no
serde default, so it is load-bearing. Renaming without
`#[serde(alias = "value_list_size")]` and
`#[serde(alias = "disable_listing")]` breaks the deserialization of
every golden universe and takes the scoreboard with it. `alias` does
compose with `deny_unknown_fields`. `value_sampling` is absent from
every golden universe, so removing it is deserialization-safe.

**Dropping a YAML key loses the author's setting quietly, not loudly.**
`unknown_keys` in `crates/project/src/lib.rs:453` warns and ignores, so
removing `disable_listing`, `disable_value_listing` and `value_list_size`
from `FIELD_KEYS` would not refuse a project — it would silently
discard what the author wrote and emit a line in a warnings list
nobody reads. That is worse than a refusal. So the old spellings stay
in `FIELD_KEYS` and keep working, mapped onto the new fields, with a
deprecation warning that names the new key. `values:` stays accepted
too, ignored with a warning, because its declaration has never had any
effect: nothing reads `value_sampling` today, and nothing writes the
sample file, so an author who set `values: full` loses nothing real
when it goes.

**Two literals and eight fixtures.** `crates/planner/src/pruner.rs:188`
emits `"value_list_size": 0` inside a synthetic field it builds as
JSON, and `docs/ossie.md:117` names the key in prose. Eight files under
`fixtures/projects/grocery-store/models/` set `disable_listing:`, so
the repository's own fixture project starts warning in phase A unless
they are updated — and they are fixtures, not goldens, so updating them
is allowed.

### What the authoring surface has to say

The change is not done when the code compiles. Three places speak to
people and to agents, and all three name the old keys:

- `crates/cli/src/templates.rs` — two separate edits. The commented
  field block shows `values: sample  # sample | full | none`, and
  becomes `sample_size`, `disable_sampling` and `sample_sql` with the
  string-only default stated, since this template is where most
  authors learn a key exists. And the `datasources.yml` preamble says
  of `host`, `port`, `database` and `username` that "every one of those
  keys is ignored and none of them is kept" — which `zsql introspect`
  makes false. It has to say instead that 0sql reads them locally to
  connect, that the deploy still uploads them, and that only the
  secret-shaped names are stripped.
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

The comparison is not string equality. `crates/planner/src/sql/expr.rs`
already encodes the asymmetry that makes this hard: a bare identifier
matches case-insensitively while a quoted one matches exactly, per the
dialect's own quote pair. So `check` folds the way the datasource's
dialect folds — Snowflake upper, Postgres lower, BigQuery and
ClickHouse case-sensitive — and a `physical_name` the author quoted is
compared verbatim. Getting this wrong produces a wall of false
positives on the first run, which is how a checker gets turned off.

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

Phase A is the only one with a hard internal order, because the limit
fix and the format bump both have to land before anything writes a
snapshot or runs a statement.

- [ ] **A1 — the dialect-rendered row limit.** `LIMIT`, `TOP n` in the
      `SELECT` clause, and `OFFSET … FETCH` where an `ORDER BY` exists,
      through the dialect rather than three `format!` calls.
      `crates/golden/tests/limit.rs` becomes a per-dialect assertion.
      *Gate:* the golden scoreboard unchanged **and** `limit.rs`
      covering every adapter.
- [ ] **A2 — the keys.** `disable_sampling`, `sample_size`,
      `sample_sql`; `values:` and `ValueSampling` removed; the
      string-only default with an absent `data_type` off; `hidden`,
      `secure` and policy-named fields excluded; `FORMAT = 2`; the
      serde aliases the goldens need; the `pruner.rs` literal; the old
      YAML spellings kept in `FIELD_KEYS` and deprecation-warned; the
      eight grocery-store fixtures; `docs/ossie.md`; the three
      authoring surfaces. *Gate:* the scoreboard unchanged; a test that
      loads a project using every old spelling and warns once per key
      while still applying the setting; a snapshot round-trip test that
      a `FORMAT = 1` snapshot still reads.
- [ ] **B — connect and read.** The connection from `datasources.yml`,
      `.zsql` and `ZSQL_DS_<KEY>_*`; a `Catalogue` type;
      `zsql introspect` writing `catalogue.json` for two adapters
      (DuckDB and Postgres), with a timeout, a row cap, and no
      statement that is not a read. *Gate:* binary growth and clean
      build time measured against `docs/execution-engine.md`'s table.
- [ ] **C — model from catalogue.** Generation with marked inferences;
      diff-and-patch on re-run with `--dry-run` the default for an
      existing project; `zsql check` against a catalogue with
      dialect-correct folding; a drift report between two catalogues.
- [ ] **D — sampling.** Column and plain-`sql` dimensions only, with
      calculated, lookup and array dimensions skipped with a reason;
      partitioned tables requiring an explicit window; batched distinct
      counts; the `sample_size` ceiling enforced at write; the budget,
      the estimate and `--dry-run`; `sample_sql`, never executed from a
      deployed project; and the governance rules written into the
      command, the template and the file's documentation.
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
- Whether any deployed project has a `samples/values.jsonl.gz` from
  the old external tooling, and whether any of those records carry a
  `range`. If none do, `range` can leave the record rather than
  staying unwritten, and `show.rs` simplifies with it.
- Whether a deployed snapshot anywhere predates `FORMAT = 1`, which
  decides whether the bump needs a migration path or only the `Newer`
  message.

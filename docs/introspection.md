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

## The model changes, as landed

There is no production use of the service, so nothing here is a migration:
the keys are gone rather than aliased, and the goldens were rewritten to
match in their own commit before the code changed.

**Six keys left the field set**, none of which anything read:
`display_type`, `secure`, `disable_listing`, `disable_value_listing`,
`value_list_size` and `values`. The planner never consulted one,
`Model::values_of` consulted neither listing key, and `fields()` never put
`display_type` or `secure` in an answer, so this removed a surface and not
a behaviour. `display_type`'s only teeth were a validation refusing a value
that nothing then used.

The `display_type` that remains in 17 golden case files is a different
key: it sits in `spec.projections`, where a caller sends it and nothing
reads it. It is not the model's and it stays.

**Three keys arrived on a dimension.**

| key | default | what it means |
|---|---|---|
| `enable_sampling` | `false` | collect this dimension's values. True is obeyed whatever the field is |
| `sample_size` | 1000, at most 20000 | how many values, most frequent first |
| `sample_sql` | none | a query the modeller wrote, value in the first column, optional count in the second |

**There is no third state and no type rule in the loader.** An earlier
draft of this plan had sampling default on for `string`, off elsewhere, and
an `Option<bool>` so an author could override either way. That is gone:
`enable_sampling` is a plain boolean, off until asked for, and asked-for
means sampled. The modeller knows the data and the loader does not, so the
loader does not guess and does not argue.

**Where the judgement is written down is `AGENTS.md`**, which `zsql init`
now generates into a project beside `project.yml`. It says to enable
sampling only on short string dimensions under 255 characters, and says why
long text is the mistake to avoid: prose in the catalogue matches every
request a little and nothing well, which is worse than having no samples at
all. Keys and identifiers, numbers, dates, booleans and anything a policy
covers stay off.

**The sampler warns rather than refuses.** When what it collected looks
like long text it says so, naming the field, because that is the signal
that a dimension wants turning off or wants a `sample_sql` that trims and
filters what it returns. There is no truncation length and no format
detection: a modeller who needs either writes the query.

**`sample_size` has a ceiling of 20000, refused at load** with the field
and the bound named. The reason it is a refusal and not a clamp:
`ValueCatalog::read` refuses a file past its inflated limit and
`crates/project/src/lib.rs` turns that refusal into a warning with *no
values at all*, so one greedy dimension would cost every other dimension
its values.

**Completeness is inferred, not declared.** `DimensionValues.distinct` is
already the column's total and `values.len()` is what came back —
`crates/cli/src/show.rs` already prints "N of M values" from the two. The
record gains `sample_size`, the limit the query ran with, which matters in
one case: when the total was refused by budget, a list as long as the limit
is the only sign that the sample is truncated rather than complete. `mode`
left the record with the `values` key that fed it.

**The snapshot format is 2.** `crates/semantic/src/snapshot.rs` says to
raise it "in the same change that makes a snapshot a reader of the previous
number could not understand: a field added without a serde default, or one
renamed or removed" — this did both, so a stale snapshot is told to deploy
again rather than failing on a serde error.

### The generated sampling query

When a dimension has no `sample_sql`, it goes through the planner like any
other query. The command writes one expression line per dimension:

```
<dimension>, calc(count(*), integer) as n desc, limit <sample_size>, hints (<lookup tables>)
```

and runs the statement the planner returns on the datasource the planner
chose, through the same transport `introspect` uses. The distinct total is
the same line without the order or the limit, wrapped as a derived table
and counted; the plan's `root_at` keeps any common table expressions in
front of the wrapper. Most frequent first, which is what the record format
stores. Four things about it are worth knowing.

**The row limit is now the dialect's**, and that landed first.
`Plan::sql_with_limit`, `dag.rs` and `sql/builder.rs` each appended
`"LIMIT n"` with no dialect indirection, and SQL Server has no `LIMIT`, so
a spec carrying a limit against a `sqlserver` datasource planned to a
statement that would not parse. `base.yml` now carries `limit_style` and
`limit_clause`; `sqlserver.yml` overrides them with `top` and `TOP (@n)`,
which needs no `ORDER BY` where `OFFSET … FETCH` would; and
`Dialect::apply_limit` takes the offset at which the statement begins, so a
limit lands on the statement a plan ends with and never on a common table
expression in front of it. `Plan` carries that offset as `root_at`, which
`final_query` reports because it is the one place that knows. The third
site was dead and is gone.

**Which table answers is the planner's decision, not the command's.** A
partitioned table in this model is a more recent cache of the same rows,
and the table a dimension is declared on is not necessarily the one it
should be read from. The planner already ranks candidates — partitions,
lookups, row estimates — so the command does not re-decide any of that.
Tables whose expression for the dimension is marked `lookup` are passed
as `hints`, from which the planner may choose; in the end the planner
decides, or the author does with `sample_sql`. The same goes for the
expression itself: a calculated dimension, an array, a plain `sql`, are
all the planner's to render, so none of them is skipped. What the command
skips is only what the model's own rules forbid — a hidden dimension, one
a security policy names — and a planner refusal is reported as a warning
on that dimension, not a reason to stop.

**Ties are the warehouse's.** The planner lays dimensions before
calculations and orders by the projections in that order, so the count
cannot lead an `ORDER BY` that then breaks ties by value. The order is the
count alone; which of several equally frequent values a limit cuts is
whatever the warehouse returns. The written file is still sorted, by
count then value, so two runs over the same values produce the same
record.

**`COUNT(*)` counts the rows of the table the dimension lives on.** On a
dimension table that is dimension rows, so a near-unique column returns
`n = 1` for every value and "most frequent first" degenerates to arbitrary
order — which `show.rs` and `crates/cli/src/assist.rs` both present as
meaningful. Weighting by the fact table needs a join this template cannot
express, so the frequency is documented as "rows of this dimension's own
table" and a fact-weighted count is a later question.

**`sample_sql` is never executed from a deployed project.**
`crates/cli/src/project.rs` adds every `models/**/*.yml` byte-for-byte, and
stripping a key inside a model file would mean parsing and re-emitting all
of them, destroying comments and key order in files that are mostly
comments. So the rule runs the other way: there is no server-side sampling
path today, and if one is ever built it ignores `sample_sql` and samples
only what it can generate itself.

### What the authoring surface says now

- `crates/cli/src/templates.rs` — the commented field block shows
  `enable_sampling`, `sample_size` and `sample_sql`. Note for whoever edits
  it next: the `\n\` line continuation these templates are written with
  eats the leading whitespace of each line, which is why `AGENTS_MD` is a
  raw string and why the field block's indentation does not survive into
  the generated file. Harmless for YAML comments, fatal for a markdown
  list.
- `AGENTS.md`, generated by `zsql init`, carries the sampling guidance.
- Still to change when introspection lands: the `datasources.yml` preamble
  says of `host`, `port`, `database` and `username` that "every one of
  those keys is ignored and none of them is kept", which `zsql introspect`
  makes false; and `crates/server/src/mcp_prompts.json`, whose `values`
  tool description says samples arrive by upload and should tell an agent
  that "no sample because sampling is not enabled" differs from "this
  project has none".
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

Write `samples/values.jsonl.gz` from the warehouse, over the dimensions
whose `enable_sampling` is on, each with its own `sample_size` and its own
or the generated query. There is no further eligibility rule: the model
said which dimensions, and the sampler takes it at its word and warns when
what came back looks like long text.

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

## The adapter surface: three transports, not thirteen adapters

The thing that makes this phase affordable, and the axis the phases below
are cut along. Executing needs thirteen result protocols; introspecting and
sampling need one statement each, and what varies between the thirteen is
almost entirely *how you connect*, not what you ask.

**The query is nearly uniform.** `INFORMATION_SCHEMA.TABLES` and
`.COLUMNS` exist on twelve of the thirteen. SQLite's `PRAGMA` is the only
genuine exception; the others are narrow preferences rather than different
worlds — ClickHouse also has `system.tables`, Redshift's `SVV_*` views see
external and late-binding tables that `INFORMATION_SCHEMA` does not, Druid
implements a subset, and Athena's catalogue is really Glue. Sampling is
`SELECT`, `GROUP BY`, `COUNT`, `ORDER BY` and a limit: the dialect layer
that already exists, pointed at two fixed statements.

The part that is not uniform is **comments**. Column and table
descriptions are the highest-value field in a catalogue — they are the
natural-language bridge a model needs — and each warehouse exposes them
differently or not at all. Expect a per-adapter query for that one field
even where the rest is shared.

**The connection is the real work, and it comes in three kinds.**

| transport | adapters | what it costs |
|---|---|---|
| one HTTP client, bearer or SigV4 header | **8**: Snowflake, Databricks, BigQuery, Trino, Druid, ClickHouse, Athena, Redshift Data API | hyper and rustls are already in the tree; a few hundred lines of request and response types each |
| three pure-Rust wire crates | **4**: Postgres, MySQL, SQL Server; Redshift reachable either way | `tokio-postgres`, `mysql_async`, `tiberius`, measured together at about +27 s clean build and +3 MB stripped |
| in-process libraries | **2**: SQLite, DuckDB | `rusqlite` is already in the lock via accounts; DuckDB is 317 s and 41 MB and needs a musl C++ toolchain the release workflow does not have |

So it is roughly three units of work for all thirteen rather than thirteen
units. Once a statement can be POSTed to one HTTP warehouse and JSON rows
read back, the same shape serves the other seven, and it serves the
catalogue query and the sampling query alike because both are just
statements.

**What splits the thirteen is not difficulty but verifiability.** Five
cannot be tested without a real account and a real bill: Snowflake,
BigQuery, Databricks, Redshift and Athena. The other eight run here and in
CI — Postgres, MySQL, SQL Server, ClickHouse and Trino in containers,
SQLite and DuckDB with no server at all — and between them they exercise
**all three transports**, HTTP included, through ClickHouse and Trino. That
is why the phases below build every transport at once against containers,
and leave the cloud five last: they are mechanical additions to a transport
that already works, held back for want of credentials rather than for want
of code.

The repository has no container infrastructure today — no compose file, no
Dockerfile, and CI runs none — so that is new work in phase B, and it is
what makes the remaining adapters cheap instead of frightening.

## Phases

- [x] **A1 — the dialect-rendered row limit.** `limit_style` and
      `limit_clause`; `TOP (@n)` for SQL Server; `apply_limit` with the
      offset where the statement begins; `Plan::root_at`; the dead
      `Builder` limit and offset removed. The dialect crate walks every
      adapter and asserts it takes a limit, that SQL Server takes `TOP` and
      no suffix, that the CTE offset is honoured, and that a statement the
      splice cannot land in is named rather than mangled; the golden suite
      plans every case it can and asserts the offset a plan reports is the
      offset its statement has and that the limit appears once. Scoreboard
      unchanged at 385.
- [x] **A2 — the goldens, on their own.** The six keys stripped from all 82
      universe files, in place, since the universe hash is only a filename
      and is never checked against the content. `import_goldens.py` drops
      them on the way in, because strata still writes them.
- [x] **A3 — the keys.** Six out, `enable_sampling`, `sample_size` and
      `sample_sql` in, the ceiling refused at load, `FORMAT = 2`,
      `DimensionValues.mode` gone, the nine `disable_listing` lines in the
      grocery-store fixtures removed, the field template updated, and
      `AGENTS.md` generated by `zsql init`.
- [x] **B1 — the connection, and one transport.** A new crate holding a
      `Connection` read from `datasources.yml` with its secret from `.zsql`
      or `ZSQL_DS_<KEY>_*`; a `Catalogue` type; and a `Source` trait with
      one method that runs a read-only statement and returns rows. The wire
      transport first, on `tokio-postgres`, because it is the one that
      settles how a credential, a timeout, a row cap and a TLS
      configuration are expressed. No statement that is not a read, a
      timeout defaulting to the `query_timeout` already on `Datasource`,
      and a row cap on every call. *Gate:* binary growth and clean build
      time measured against `docs/execution-engine.md`'s table, and the
      slim build (`--no-default-features`, which CI checks) still compiles
      — tokio enters it here, which is a measurable change and not a free
      one.
- [x] **B2 — the containers, and the other two transports.** Six
      adapters across three transports, every one proven against a real
      server by the CI job: Postgres, MySQL and SQL Server on the wire;
      ClickHouse and Trino over HTTP; SQLite in-process. `zsql introspect`
      writes `catalogue/<datasource>.json`, one file per datasource.
      Measured with six linked: 16.5 MB stripped, +2.7 MB; a clean release
      build of 77 s, +8 s; the slim build compiles with tokio in it;
      `cargo deny` passes and ring is the only crypto provider. The gate
      held: the same catalogue assertions pass against every container,
      and the three adapters written without a server needed one fix each
      — MySQL answers an unaliased `information_schema` column under its
      upper-case name, and `sys.objects` lists user objects only.

      What the containers taught, kept for whoever adds the seventh: the
      job ran four times before it was green, three of them lost to one
      probe. Trino's server started in fourteen seconds every time; a
      `curl` against `/v1/info` passed once and then never, and nine
      minutes of "not healthy" could not tell a slow JVM from a dead
      probe. The diagnostics step — `compose ps`, free memory, each
      container's log tail and inspect line, on failure only — is what
      answered it, and the probe is now the image's own
      `/usr/lib/trino/bin/health-check`. Probe with what the image
      promises, and when a container will not come up, print what it said
      before tearing it down.

      **`sslmode`, landed, and the last of B2.** libpq's five rungs under
      libpq's names, with libpq's meanings, because that is the ladder
      every warehouse client shares — MySQL's `--ssl-mode` is the same
      five under other names, and SQL Server's `Encrypt` with and without
      `TrustServerCertificate` maps onto three of them. `require` encrypts
      and does not verify, as it does in libpq: the rung a self-signed
      development server needs, named rather than hidden behind a flag.
      The default is `verify-full`, where Microsoft moved its drivers'
      default in 2022 and what the PostgreSQL documentation recommends
      over its own `prefer`, which it says "makes no sense from a security
      point of view". `prefer` is accepted, never defaulted to, and a
      connection that ends plaintext under it says so in its warnings,
      which `introspect` prints. The trust store is the platform's, with
      the Mozilla bundle when the platform has none and `ca_file` on top
      for a private CA; `ca_file` is also read as `sslrootcert` and
      `ssl_ca`. Over HTTP the ladder collapses to two questions — is plain
      `http` allowed, and is the certificate checked — since the scheme
      already encrypts. Errors carry their cause chain, so a refusal under
      the default against a server with no TLS says "server does not
      support TLS" and not only "handshake".

      What it cost, measured as this plan asked. SQL Server's TLS brings
      rustls's default provider, aws-lc, in beside ring through tiberius's
      `tokio-rustls` dependency, which has no switch for it: 2.7 MB of
      binary, deny-clean, with `ring` installed as the process default
      once so the two coexist and every configuration here names its
      provider. MySQL moved from the synchronous `mysql` crate, whose TLS
      feature dragged in the old `webpki` and the unmaintained
      `rustls-pemfile`, to `mysql_async`, whose tree is ring and the bundle
      alone. TDS stays 7.3; `strict` can come with 8.0.

      Proven by the containers on CI, after a detour: the Postgres TLS test
      had passed vacuously while its fixture was unmounted, and once run for
      real it found the transport's one bug (binary cells read as null) and
      a fixture that was a CA certificate presented as a server's. Postgres
      now serves a certificate signed by a private CA in
      `harness/sources/tls/`, so `require` is shown to accept it unverified,
      `verify-full` to refuse it, and `verify-full` and `verify-ca` with
      `ca_file: ca.crt` to accept it; MySQL and SQL Server make themselves such
      certificates, so the same two rungs are shown on each. The Postgres
      container publishes on 5466, since a developer's own Postgres tends
      to hold 5432. *Gate, held:* the same catalogue assertions pass
      against every container, so a thirteenth adapter is a file and not
      a project.
- [ ] **C — model from catalogue.** `zsql generate` landed, on four
      decisions taken as follows.

      *Never overwrite.* A table is matched to an existing model by
      `physical_name`; when one exists, `generate` lists the columns the
      catalogue has and the model lacks, with the exact command to add
      them — `--columns a,b` for some, `--new` for all — and adding
      appends field entries to the file's `fields:` block and touches
      nothing else, or prints them to paste when the file is not shaped
      for that. A column with no 0sql type is named once as such and never
      counted as new. Flags and a listing rather than a prompt, since
      agents and CI run it too.

      *Sampling from width.* `enable_sampling: true` on a string declared
      255 or narrower, with the width on the line; nothing else touched.
      The loader stays rule-free; generation is a different layer, and
      this is the judgement `AGENTS.md` asks a modeller to make.

      *Types.* Known types mapped per adapter; the text-shaped unknowns —
      `uuid`, `json`, `jsonb`, `xml`, enums, `inet` and kin — to `string`
      and said so; anything else, arrays and intervals and geometry,
      skipped and listed in the file's header and the command's output,
      so the hole is never silent. The warehouse's own spelling rides on
      every `data_type` line.

      *Measures.* Numeric columns whose names look additive become `sum`
      measures; keys — the primary key, foreign-key columns, anything
      called `id` — stay dimensions; everything else is a dimension.

      "Marked as inferred" is a trailing YAML comment on the line, which
      the never-overwrite rule is what makes safe: comments survive
      because nothing re-emits the file. Joins come from foreign keys
      into one `rel.<schema>.yml` per schema, written once; a later run
      names any join the file lacks. `cost` is bucketed from the row
      estimate. The planner queries a generated model unaided — a `sum`
      across two generated tables on the warehouse's foreign key — and
      the real loader validates every draft the SQLite and Postgres
      fixtures produce.

      Remaining: `zsql check` against a catalogue with dialect-correct
      folding, and a drift report between two catalogues.
- [ ] **D — sampling.** `zsql sample` landed: every dimension with
      `enable_sampling`, through the planner like any other query — the
      dimension, `calc(count(*), integer)` descending, the dialect's own
      row limit at `sample_size`, lookup tables as hints — or the author's
      `sample_sql` as written, value in the first column and an optional
      count in the second. The distinct total is the same plan counted as
      a derived table, so a record says whether its list is complete;
      `sample_size` rides on every record. Hidden dimensions and
      policy-named ones are skipped with their reasons; everything else is
      the planner's to answer or refuse, and a refusal is a warning. The
      command says its plan — dimensions, queries — before running,
      with `--dry-run` to stop there, `--budget` to refuse past a count,
      and `--only` for one dimension or table, keeping the rest of the
      file. The long-text warning fires on what came back and on what the
      catalogue declared. Writing it found the Postgres transport's one
      real bug: binary cells read as strings came back null, so every
      non-text value was lost; it speaks the simple-query protocol now,
      and the transport test asserts an integer, a boolean, a numeric and
      a date arrive as text.

      The long-text warning fires past an average of 100 characters or a
      declared width past 255, one constant each; ruled fine as is.

      Proven on CI: the SQLite flow in the ordinary suite, and the
      Postgres fixture through a model `generate` drafted from it — the
      width it turned sampling on from, real values most frequent first
      with their distinct total, and the warning the catalogue's declared
      `text` raises when a column is turned on by hand. Remaining: the
      governance rules written into the command's output and the sample
      file's documentation.
- [ ] **E — the cloud five.** Snowflake, BigQuery, Databricks, Redshift and
      Athena. The shape below comes from reading how strata and its `dwh`
      gem connect to four of them (see "What strata and dwh do"); the
      plan's earlier line, "the HTTP transport plus SigV4 for the two AWS
      ones", was half right. Redshift is Postgres on the wire and gets the
      Postgres transport; only Athena needs SigV4. Each of the rest is one
      request type over the HTTP client B2 built, a poll loop, a result
      pager, a catalogue query and a comments query. What they are waiting
      for is accounts to verify against, not code. DuckDB sits here too,
      behind its own decision about the musl C++ toolchain.

      The proposed shape, one line each:

      | adapter | wire | auth the CLI offers | catalogue |
      |---|---|---|---|
      | Snowflake | `POST /api/v2/statements`, 202 + `statementHandle` poll, `?partition=N` pages | `personal_access_token`; `username` + `private_key` (RS256 JWT, 1 h) | `<database>.information_schema`, comments on tables and columns |
      | Databricks | `POST /api/2.0/sql/statements` with `warehouse_id`, `wait_timeout 30s`, `on_wait_timeout CONTINUE`, `JSON_ARRAY` inline, `/result/chunks/N` | `personal_access_token`; `oauth_client_id` + `oauth_client_secret` (client credentials at `/oidc/v1/token`, scope `all-apis`) | `<catalog>.information_schema`, comments present |
      | BigQuery | `POST /bigquery/v2/projects/<p>/queries`, `jobs.getQueryResults` pages, `useLegacySql false` | `key_file` (service-account JSON: RS256 JWT bearer grant); `application_default_credentials.json` (refresh token) | `<project>.<dataset>.INFORMATION_SCHEMA`, `OPTIONS` descriptions |
      | Redshift | Postgres transport, port 5439, `sslmode` as today | `password`; later IAM temporary credentials | `svv_table_info` for rows, `pg_description` for comments, `svv_columns` for widths |
      | Athena | SigV4 to `athena.<region>.amazonaws.com`: `StartQueryExecution`, `GetQueryExecution` poll, `GetQueryResults` pages (first row is the header; every cell is `VarCharValue`) | `access_key_id` + `secret_access_key` (+ `session_token`); the `AWS_*` variables; a profile in `~/.aws/credentials` | `information_schema` over Glue, `s3_output_location` and `workgroup` required |

      Snowflake landed first, on the shape above and without an account:
      the transport is proven against a fake of the statements endpoint
      on a loopback socket — submit, 202 and poll, partitions, the cap
      from the total before any partition is fetched, the server's own
      message on a refusal — and the key-pair signer against the
      fingerprint openssl prints for the same key, the token's claims and
      signature checked with the public key. The catalogue reads
      `INFORMATION_SCHEMA` with every alias quoted and the keys from two
      `SHOW`s mapped into the shared shape. `Secrets` replaced the single
      password: every field `.zsql` may hold, resolved once, and the
      adapter names everything it is missing in one error. What a real
      account will tell: whether `SHOW … IN DATABASE` needs the name
      spelled, how a statement with no warehouse answers, and the
      `numRows` of a `SHOW`.

      Decisions the shape rests on, each of which is the user's; all five
      were agreed on 2026-10-21:
      1. Redshift over the Postgres wire, not the Data API. It is what
         dwh does, it reuses a transport that is proven, and it leaves
         SigV4 to Athena alone.
      2. No browser OAuth in the CLI. Snowflake and Databricks offer a
         token and a machine credential; the authorization-code flow that
         strata runs in a web app is a server feature, not a CLI one.
      3. Athena reads the standard credential chain in its first three
         links only — explicit keys, the environment, a profile file. No
         instance metadata and no SSO cache until someone needs them.
      4. BigQuery is 0sql's own: neither strata nor dwh has it, so there is
         no prior art to copy and the two credential files are the design.
      5. Verification needs an account on each. None exists in CI today;
         which of the five the user can provide decides the order.
- [ ] **F — the corpus**, as its own decision and its own document, drawing
      on the goldens, the refusals and the corrections as much as on the
      samples.
## What strata and dwh do

Strata is a Rails app whose connections run through the `dwh` gem, and
the strata CLI writes the same connection YAML for it. Four of the cloud
five are there; BigQuery is not. What transfers, and what does not.

**The transport pattern is the one B2 chose.** Snowflake, Databricks and
Athena are HTTP in dwh too: submit, poll with a backoff from a quarter
second to thirty, page the result. Snowflake pins every date and time
output format in the request's `parameters` so cells arrive as text, which
is the same decision the Postgres transport made with the simple-query
protocol. Athena's `GetQueryResults` returns every cell as a `VarCharValue`
with the header as the first row of the first page. Databricks' result
comes as a manifest of chunks fetched one by one. None of this needs an
SDK except Athena, where dwh leans on `aws-sdk-athena` for signing and for
the credential chain; 0sql signs by hand, so the chain is the cost.

**Redshift is a Postgres subclass**, forty lines: port 5439, the `pg`
driver, a `date_add` override. The plan had it on the Data API with SigV4;
the wire is simpler and already built. What Redshift needs of its own is
the catalogue: `reltuples` is not maintained, `svv_table_info` is, and
`pg_stat_ssl` does not exist for the `prefer` warning.

**Credentials are per adapter, by name.** The strata CLI's `ds auth` asks
for a different set per adapter and writes it under the datasource's key
in `.strata`, chmod 600: Snowflake `auth_mode` with a
`personal_access_token` or a `username` and `private_key` path; Databricks
`oauth_client_id` and `oauth_client_secret`; Athena `access_key_id` and
`secret_access_key`, with `session_token` through the passthrough map;
everyone else a `password`. `.zsql` is already that file, and `secret(dir,
uid, field)` is already field-generic, so phase E is new field names, not a
new mechanism; the `ZSQL_DS_<UID>_<FIELD>` variable extends the same way.
The server side stores each as an encrypted column and keys its connection
pool on a digest of the config with rotating tokens excluded, which is the
right split for `zsqld` if it ever connects.

**Validation up front, with the whole list.** dwh declares each adapter's
keys with `required`, `allowed` and a message, and strata's model reports
every missing one at once rather than the first. `open()` should do the
same per adapter: the full list of what is missing for this `adapter` and
`auth_mode`, in one error.

**Key-pair auth is a JWT.** Snowflake's is RS256 with `iss` of
`ACCOUNT.USER.SHA256:<base64 of the public key's DER digest>`, `sub` of
`ACCOUNT.USER`, an hour's `exp`; BigQuery's service-account grant is the
same machinery with different claims. One signer serves both, and `ring`
is already in the tree.

**OAuth is where the copy stops.** dwh's `OpenAuthorizable` carries the
authorization-code flow with PKCE, a refresh loop and a token store the
host app persists on the datasource row. That is a web app's shape: a
redirect URI, a callback, a session. A CLI mints a client-credentials
token per process and is done; the token store has nothing to store. The
strata CLI reached the same conclusion, offering only `oauth_m2m` for
Databricks.

**Not worth copying.** dwh interpolates identifiers into its catalogue
queries and upper-cases Snowflake names itself, where 0sql's dialect
already knows the fold; the retry wrapper on `execute` re-runs a query
that may have cost money; `ssl: true` on Redshift is a boolean where the
rest of 0sql spells `sslmode`.

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
- Hosted minutes are finite. `bin/ci` runs the whole verification set
  here, the sources step included, and signs the commit off with
  `gh signoff` when it all passes, as strata's `bin/ci` does; `ci.yml`
  carries `workflow_dispatch` so the flip to local-only is one line. What
  it still needs: the docker socket on this machine, and `gh signoff
  install` once so `main` requires the sign-off.
- The five images are `latest` or a major tag, which is how Trino's
  probe broke between one run and the next. Pin each to the version the
  green run pulled, and bump them on purpose; the diagnostics step prints
  what a container is running, which is where to read the versions from.
- Whether `range` should leave `DimensionValues` rather than staying
  unwritten. Ranges are out of scope for now rather than never, and
  `show.rs` branches on the field, so it is kept; it is dead weight until
  something writes one.
- Whether the sampler's long-text warning should fire on the declared
  column width from the catalogue as well as on what came back. The width
  is the stronger signal and it is free once introspection reads
  `CHARACTER_MAXIMUM_LENGTH`, but it only exists after phase B, and it is
  a warning either way.

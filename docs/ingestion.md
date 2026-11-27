# Data prep: datasets beside the model

A plan, started 2026-11-26, revised after a review the same day. High
level only; nothing here is built or scheduled. One node only; more than
one is out of scope.

**Next shape, planned 2026-11-27:** [Sources, datasets and pipelines](#sources-datasets-and-pipelines-planned)
replaces a dataset's linear recipe with a graph the application sends
and 0sql runs. What follows it describes what is built today; that
section says what of it carries over.

## What it is for

The model describes the warehouse as its developers built it. A
**dataset** is the data that is not there: a list a user uploads, a
slice of the model they want to reshape, budgets kept in a spreadsheet.
**Data prep** is small-scale, last-mile ETL over such data: bring rows
in, shape them step by step, combine them, and use the result on its
own or blended with the model.

Data prep is **independent of the semantic model.** It has its own
crate, its own formula language and its own engine. It reads the model
only through the ordinary planner, as any caller does, and the model
never reads it.

A dataset is **a recipe over sources**:

```
 sources ──► snapshots ──► recipe (steps 1..n) ──► materialization v<k> ──► query, blend, feed another dataset
 upload         immutable      replayed on every       Arrow, immutable,
 semantic query                new snapshot            records its lineage
 dataset
 feed (later)
```

- A **source** brings rows; each time it does, they are kept as a
  **snapshot**.
- The **recipe** is an ordered list of **steps** that shape them.
- A **materialization** is the recipe run over the latest snapshots,
  kept as an Arrow IPC file, as results are: what the grid shows and
  what is queried.

Editing in the grid adds to the recipe; stored rows are never changed.
A sync brings a new snapshot and runs the same recipe over it, which is
how every edit is applied again.

The warehouse stores nothing. A semantic source is the one thing that
reads it, and it only reads.

**Datasets are small.** A dataset is capped in rows and bytes, at import
and at every materialization, so that the server's own engine never
holds more than it was sized for. A source or a step that would pass
the cap fails, saying so.

## Who may use it

- **The server keeps no user.** A dataset belongs to an org and a
  project and is addressed by its id, as a deployed branch or a result
  is. Who may read, change or share which dataset is the application's
  to decide, with its own records of who made what; the server answers
  whoever reaches it, as it does for everything else.
- **Sharing is the application's.** A user shown a chart built on a
  dataset reads that dataset because the application lets them; the
  server is not asked, and there is nothing to grant. (An owner header
  scoping datasets per user was built first, and dropped on 2026-11-26
  for this: it duplicated the application's access control and fitted
  no sharing model.)
- **What a semantic dataset holds is its creator's view.** Its snapshot
  is what the creator's context could see; whoever the application lets
  read it sees those rows, whatever their own context. The application
  should say so before such a dataset is shared.
- **Who may create one is decided in front** too: by the routes the
  application forwards. `ZSQL_DATASETS=off` turns the feature off as a
  whole.
- **Per project, not per branch.** A deploy never takes a dataset away.

## Sources

| source | a snapshot is | taken when |
|---|---|---|
| upload | the parsed file | each upload, the first and every replace |
| semantic query | the result of a spec run on a branch | the dataset is refreshed |
| dataset | another dataset's materialization | that dataset has a new one |
| feed (later) | what an upstream system pushed or a connector pulled | each sync |

Each ends in a snapshot, an Arrow IPC file with a manifest, so the
recipe never knows which kind fed it.

**A CSV upload** says, or has detected and recorded: encoding (UTF-8,
with a BOM accepted), delimiter, quote, whether there is a header. Types
are inferred, and a column that looks like an identifier (leading zeros,
postcodes) stays text. Column names are made unique and case-distinct;
an empty name becomes `column_<n>`. A row that does not parse is counted
and the first few are shown; past a threshold (one row in a hundred, or
`max_bad_rows`) the import fails rather than quietly dropping data.
Every choice is in the manifest. The file is parsed by data prep in
Rust, not by the engine.

**A semantic source** (`POST .../datasets/query` with a branch and a
`spec` or an `expr`; `POST .../{id}/refresh` again) runs its spec under
the context sent with the refresh, row policies compiled in. The server
plans and runs it as `/run` does, fresh, and hands data prep the rows;
data prep never calls the planner. Column roles come from the model:
a measure is a measure. A run that outlasts the request's wait is
answered 202 with the dataset's id and the run's, waited on after, and
the dataset says what it waits on, or that it failed. The snapshot shows what that context
could see; that is the creator's to manage. A refresh always runs
against the warehouse (`fresh`), never answered from the cache. The
server keeps no context, so a refresh is a request from the
application; a failed one leaves the last materialization in place.

## Columns have roles

Each column of a dataset is a **dimension** or a **measure**. Inferred at
import (text, dates, booleans and identifier-like columns are
dimensions; other numbers are measures) and changed by the user. A
semantic source's columns take their roles from the model. Roles decide
two things: what an edit to a cell means, and how a dataset blends.

## Types evolve, never truncate

When a new snapshot, or a join, meets a column of another type:

- **Widen when nothing is lost**: integer to a wider integer, integer
  to decimal, decimal to a wider decimal or to float, a date to a
  timestamp.
- **Narrow or reinterpret only if every existing value converts**: text
  to a date when every value parses as one, text to a number likewise.
- **Otherwise fail**, naming the column, the two types and the first
  values that do not convert.

The type a column was given by the user is kept unless the rule above
forces a change, and the change is reported.

## Steps

A **closed set** of declarative operations. Each declares the columns it
reads and writes and compiles to SQL for DuckDB. The one way to write
SQL is the `sql` step, a black box with its own rules ("The `sql`
step", below).

Step names say what they act on: a column step ends in `_column` (or
`_columns`, when it takes a list), a step that rewrites values in
`_val`, and a case step is named for the case it makes.

- **Columns:** `rename_column`, `retype_column`, `remove_columns`,
  `reorder_columns`; `add_column`, a new column computed by a formula;
  `update_column`, a column computed again by a formula, which may read
  it (`round(price, 2)`), its type becoming the formula's.
- **Rows:** `filter`, `sort`, `dedupe` (remove duplicates),
  `fill_empty` (a value given), `fill_down` (the value above).
- **Values:** `find_and_replace_val` (within a value, or the `whole`
  value), `remap_val` (whole values made others), `trim_val`, and the
  cases `upper_case`, `lower_case`, `title_case`, `snake_case`
  (`Grocery Store` and `GroceryStore` to `grocery_store`) and
  `camel_case` (to `groceryStore`); each names its `columns`, all text,
  and each is a formula function too: `snake_case(trim(name))`.
- **Shape:** group and aggregate (`group`: sum, min, max, avg, count,
  count_distinct), append (`append`), columns made rows (`unpivot`).
- **Combine:** join or look up another dataset (`join`, `left` or
  `inner`, with `on` keys, the `columns` brought and a `prefix`).
- **Edits:** see "Editing a cell".

A step is JSON with an `op`: `{"op": "remap_val", "column": "region",
"values": [{"from": "NY", "to": "New York"}]}`. A step with any other
`op`, or a field its op does not have, is refused. A `retype_column` keeps a
value only if it converts: text read as a number or a date (with a
strptime format when it is not ISO), a number narrowed only when it
comes back the same; the run fails naming the step, the column, both
types and the first values that do not.

**Spreadsheet shapes.** Two steps turn what people lay out for reading
into a table:

- `unpivot` makes columns rows: a budget with a column per month
  becomes `dept, month, amount`, which joins to a model's month.
  `{"op": "unpivot", "keep": ["dept"], "name": "month", "value":
  "amount"}` unpivots every column but `dept`, so a month a later file
  adds is unpivoted too; `columns` names them instead. The columns meet
  at the type they all widen to, or the step is refused, saying to
  retype them first. An empty value makes no row unless `keep_empty`.
  At most 1,000 columns.
- `fill_down` gives each empty value of the columns it names the
  nearest value above it, in the rows' order: a label written once for
  the rows below it, or the blank a merged cell leaves. Blank text
  counts as empty. A `sort` before it decides what "above" is.

**The `sql` step** is the escape hatch, for what the steps and formulas
do not say: a running total, a rank, a regular expression, a pivot.
`{"op": "sql", "query": "SELECT *, sum(amount) OVER (PARTITION BY
region ORDER BY month) AS running FROM input"}`. It is a black box:

- **It sees one table, `input`:** the rows of the step before, copied
  into a DuckDB session of its own. The source, the other steps and the
  datasets other steps read are not there, so it cannot skip the
  recipe, and another dataset comes in by a `join` or `append` before
  it.
- **It is one `SELECT`, checked by DuckDB's own parser** before it
  runs, so no quoting hides anything from the check. It reads `input`
  and the tables its own `WITH` makes (recursive ones too), and no
  other table, schema or catalog; its table functions are `range`,
  `generate_series` and `unnest`. A trailing `;` is fine. DuckDB's
  JSON extension is compiled in for the parse.
- **It is the same every run.** The clock and randomness (`now()`,
  `current_date`, `random()`, `uuid()` and the like) are refused; a
  formula's `today()` is the one clock a recipe has. So is what runs
  other SQL or reads the engine: `query()`, `getenv()`,
  `current_setting()`, `duckdb_*`, `pg_*`, `pragma_*` and `read_*`.
- **Its refusals are DuckDB's words,** without DuckDB's excerpt of the
  query, which is of the query as it was wrapped.
- **Its columns are DuckDB's answer,** learned by running it with no
  rows, each cast to a type a dataset has: a sum's HUGEINT is an
  integer; a `DECIMAL(38,0)`, such as the data's own 20-digit numbers,
  stays a decimal of no places; a decimal of more places than 18 is a
  double; a list, a struct or an interval is refused, saying to cast
  it. Names are DuckDB's too: an expression with no `AS` is named by its
  text, a repeat is numbered. A column it keeps by name keeps its role
  while it is still a number, or still not one; the rest are inferred.
- **What it reads is found when it is compiled:** a column it names
  that is not there holds the dataset as any step's would, naming the
  step and the column. Its lineage is coarse: each column it makes is
  from everything it was given.
- **It is bounded** by the session's memory and threads, and stopped
  after 60 seconds, or sooner when the job it is part of runs out of
  time. What it hands back is counted as it comes, and refused past
  twice a dataset's caps. The session has external access off and its
  configuration locked, as every session does: no file, no URL.
- **Its WITH tables are scoped as SQL scopes them:** a name made inside
  a subquery is not a table outside it, so it cannot stand in for a
  name of the engine's own (`pg_settings`) elsewhere in the query.

**Formulas have their own language.** A computed column or a filter is
written in a small expression language of data prep's own: column
references, literals, operators, and an **allowlist** of functions
(text, numbers, dates, conditionals), compiled to DuckDB's SQL by data
prep's compiler. A column is a bare name or `[any name]`; text is
quoted; `&` joins text; `and`, `or`, `not`, `is null`, `in (...)`.
Every formula is typed before it runs, every result is cast to its
type, a division by zero is empty rather than an error, and `today()`
is the time the run was pinned to. `to_number(x)` reads a number the
way people write one, giving `decimal(6)`: a currency symbol or a
three-letter code at either end, spaces and thousands separators
dropped, `(500)` as -500, `12%` as 0.12, and null for anything else;
`to_number(x, ',')` says the comma is the decimal mark, which is never
guessed. `to_integer` and `is_number` read the same way, and
`to_integer` gives null for a number that is not whole. `to_date(x)`
reads every unambiguous way a date is written: ISO dates and
timestamps, `2026/1/5`, `2026.01.05`, `20260105`, `202601` and
`2026-01` (the first of the month), and a month's name (`5 Jan 2026`,
`January 5, 2026`, `05-jan-2026`, `September 2026`). A day and a month
both written as numbers with the year last are ambiguous, so
`31/01/2026` needs `to_date(x, 'dmy')` and `01/31/2026` needs `'mdy'`;
`'excel'` reads a spreadsheet's serial number (`46027` is 2026-01-05);
a strptime format reads that one format. A date that does not exist is
null, never another date. `is_date` reads the same way, and
`to_timestamp` too, keeping a time where there is one (`2026-01-05
10:30`, `31/01/2026 14:05` with `'dmy'`, an Excel serial's fraction).
`to_boolean` reads `true/false`, `yes/no`, `y/n`, `t/f` and `1/0`,
any case, and anything else as null. `switch(x, 'NY', 'New York',
'CA', 'California', x)` gives the result for the first value `x`
equals, then the default, if one is given; `ifs(amount > 100, 'big',
amount > 10, 'mid', 'small')` the result of the first true condition. A
`retype_column` stays strict. It does not reuse `calc(...)` from the query
shorthand, which carries raw SQL. DuckDB is opened in memory with
external access off and its configuration locked, and it touches no
file at all: data prep reads and writes every snapshot and
materialization itself and hands the engine Arrow batches. No formula
and no step can read a file, a URL or another dataset's directory.

### Columns are a dependency graph

Every step declares what it reads and writes, so each column of the
result has an **ancestry**.

**A column that disappears upstream:**

- **A warning** only when nothing depends on it: the user made no step
  on it, it is not a key, and no dataset or blend reads it. It is gone
  from the result and the dataset says so.
- **Otherwise the new materialization is held**, not made live. A step
  the user made on it (a remap, an edit, a formula), a key, a filter, a
  grouping, a join, or a reader elsewhere: the dataset names the column
  and each dependent, and the user fixes the recipe or confirms. A
  rename upstream (`region` to `area`) is the common case: the user
  points the steps at the new name, and their work carries over.
  Confirming (`POST .../confirm`) makes a recipe version without the
  steps and roles that held it and runs that.

**A column the user deletes in the grid** is a remove step, checked by
the same graph first. With no dependents it is removed; with some, the
removal is refused, naming them, with the choice to remove them too.
`GET .../impact?column=` names a column's readers before anything is
changed. The rows stay in the snapshot, so undo brings the column back.

**A column that appears upstream** is added at the end and reported.

### Editing a cell

What an edit means depends on the column's role.

- **A dimension cell is a remap.** Changing `NY` to `New York` is the
  rule (column, original value) → new value, and applies to every row
  that has that value. It needs no row identity and holds through every
  sync. This is the common edit.
- **A measure cell is an override of one row,** and that row is named
  by its dimension members: the values of the dataset's dimension
  columns at the step where the edit is made. After a group step, the
  grouping columns are those members.
  - Applied when exactly one row has those members.
  - **Orphaned** (kept, listed, not applied) when none does.
  - **Refused, or held,** when more than one does: the dimensions do not
    identify the row, and nothing should guess.
  - The override stores the value it replaced, so a sync that changes
    that value upstream is **flagged**: the user's value wins and the
    dataset shows where upstream disagreed.
- **Adding and deleting rows** follow the measure rule: a row is named by
  its dimension members.

**Settled when edits were built** (they were the open questions):

- **Which rows an edit is for is the user's to choose,** by `match`:
  the columns whose values must be the edited row's.
  - **None:** every row with the edited value, a remap of the column
    (`NY` → `New York` everywhere). A dimension edit's default.
  - **Some:** the rows that agree on them (`NY` → `New York` where
    `country` is `US`), however many, through every sync; orphaned when
    none do.
  - **The row's whole identity:** that row alone. A measure edit's
    default. A measure edit whose `match` names several rows is refused
    unless it says `"all": true`.

  A dimension's own original value is always among what an edit of it
  matches on.
- **Edits apply in the order they were made,** at each place in the
  recipe: a row deleted and then its value remapped stays deleted; a
  remap of a remapped value chains (`NY` → `New York` → `NYC`); a row
  added and then deleted is gone.
- **A row's identity** is the recipe's `key` when the user marks one,
  else every dimension column. An edit that names more than one row is
  refused when it is made, saying so and offering both ways out: mark a
  key, or send `"all": true` to edit every row with those values. At a
  later sync an edit made for one row that now names several is listed
  as *ambiguous* and not applied; nothing guesses. An edit for the rows
  that agree, a dimension edit with a `match` or any edit with
  `"all": true`, is for every such row, however many matched when it
  was made.
- **Edits past a join or an append** need no rule of their own. An edit
  applies after the steps the recipe had when it was made, and names
  its row by what is there at that point. A step added later runs after
  it and sees the edited value. When the recipe changes around an edit,
  the edit follows the steps it was made after: steps put before it or
  removed from before it do not move it off what it was made on.
- **An edit is a reader.** An edit on a column gone upstream holds the
  dataset like a step does; confirming undoes it, as an entry of the
  log.

**Edits are a log, not a recipe version each.** An edit appends to the
recipe's edit log; pasting five thousand cells is one append. The log is
folded into the recipe at its position when the recipe runs. Undo is an
entry too (`{"op": "undo", "edit": "3.2"}`), and every edit's state in
the live version (applied, orphaned, ambiguous, flagged, or for an
added row, in conflict) is kept beside it and listed.

## The grid

The grid is the application's. The server answers what a grid asks:
read a page of a materialization, sorted and filtered for viewing;
preview a step over a sample; change the recipe, each change naming the
recipe version it was made against and refused against an older one;
undo, since every recipe version is kept.

## Combining datasets

A join, look-up or append to another dataset is a step whose second
input is that dataset's materialization. Datasets that read datasets
form a **DAG**:

- No cycles; a step that would make one is refused.
- A join states its keys and its kind. Key types meet by the rule in
  "Types evolve". The application helps the user define the join; the
  server refuses a side not unique on its keys rather than multiply
  rows.
- **A refresh triggers the DAG below it.** One job runs every dataset
  downstream in order, each reading the versions this job made, so a
  dataset never mixes two versions of one input. A dataset that fails
  stops its own descendants, which keep their last materialization.
  A semantic source reached by a cascade is not re-run (there is no
  context to run it under); its last snapshot is used.
- One job per dataset at a time.
- Reading an input is checked as reading it directly would be: an
  input is one of the datasets of the same project, read
  at its live version, which the lineage records.
- An append meets columns by name, each at the type both widen to; a
  column only one side has is empty on the other.
- **What a reader names is a dependency.** A join's keys and the columns
  it names are what it reads of its input. A new version of the input
  without one is held, naming the reader; confirming it lets the
  reader be held in turn when the cascade reaches it.
- A dataset others read is deleted only with `confirm=true`; they keep
  their last version and fail when run again.
  `GET .../downstream` lists what reads a dataset, in cascade order.

## Lineage

Every materialization records what made it:

- the recipe version and the edit log position;
- for each upload or feed: the snapshot, its file name, hash and parse
  options;
- for each dataset read: its id and materialization version;
- for each semantic source: the branch, the deploy, the spec, the
  planned SQL's hash, a fingerprint of the context, and the snapshot it
  produced (not the run's result id, which expires with the cache);
- the column roles and key settings;
- the versions of the server, the step compiler and DuckDB;
- the time the run was pinned to, so `today()` in a formula means one
  thing for the whole run;
- what the run skipped, held, orphaned or flagged.

And each column's ancestry through the steps to source columns and
model fields. From that: **upstream** (what this is made from),
**downstream** (which datasets and blends read it) and **impact** (what
a change would break, before it is made).

## Blending with the model at query time

`POST /projects/{uid}/branches/{branch}/blend`:
a spec whose projections may name `alias.column`, the `datasets`
attached, and the `blend` joins; `context`, `connection`, `wait` and
`fresh` as `/run` takes them. The server splits the spec: projections of
an attached alias are the datasets', the rest and the model keys the
joins name are the model's, each given an alias of its own so its
column is known. It plans and runs that as `/run` does, and data prep
joins the rest. A model part still running is answered 202; the same
request again is joined to the run. A blend's model part takes no
calculations yet.

A request attaches one or more datasets, each by id and an alias, and
**states every join itself**. Datasets are only ever there because the
request named them, and they are only ever joined the way the request
says.

```yaml
datasets:
  - { id: ds_91f, as: budget }
  - { id: ds_4a2, as: owners }
blend:
  - join: budget
    to: model
    on: [{ left: store.region, right: budget.region },
         { left: date.month,   right: budget.month }]
    kind: enrich
  - join: owners
    to: budget
    on: [{ left: budget.region, right: owners.region }]
    kind: enrich
fields: [store.region, date.month, orders.revenue, budget.amount, owners.manager]
```

- **The caller supplies the join criteria; nothing is inferred.** No
  join is found from matching names, from the model's relationships or
  from a column's role. Each join names its two sides (`model` or a
  dataset's alias), the columns on each side, and its kind.
- **Every attached dataset is joined, and joined once.** A dataset
  attached but named in no join is refused, never cross-joined. The
  joins must connect every input to the model without a cycle; a
  request that leaves an input unconnected or closes a loop is refused,
  naming it.
- **A join may be dataset to dataset** (`owners` to `budget` above), so
  a query can put a mapping beside another dataset without first
  building a combined dataset.
- **The model's part is still one planned query.** The blend planner
  asks the ordinary planner for the request's projections, and does
  every join after, in DuckDB.
- **Every model key a join names is asked for.** A dataset is at its
  keys' grain, so the answer is too. Planning a key the request does
  not show would make the warehouse send rows finer than the answer,
  only to add them up again, which is wrong for a distinct count or a
  ratio. A request that joins on `Department` and does not project it
  is refused before the warehouse is asked, saying to add it.
- **A join on no keys is a cross join** (`"on": []`, an `enrich` only):
  each row of the dataset is set beside every row of the side it joins
  to, as it is: an exchange rate, a target, a few scenarios. It is
  refused if it would make more than a million rows, and a `filter` on
  no keys is refused.
- **Kinds:** `filter` keeps the rows of the side it is joined to whose
  key the dataset has (a semi-join; the dataset need not be unique on
  the key). `enrich` brings the dataset's columns and measures beside
  that side's.
- **Each join's keys meet by the type rule** ("Types evolve"), and each
  `enrich` side must be unique on its keys or be aggregated to them,
  as in a combining step.

**Budget against actual works, as `enrich`.** The dataset's measures
(`budget.amount`) are measures of the blend. Both sides are aggregated
to the dimensions they share, and then joined:

1. The model's part is planned and run in the warehouse at the
   request's dimensions: revenue by region and month.
2. The dataset's part is aggregated by DuckDB to the same dimensions:
   budget by region and month.
3. The two are joined on them, keeping rows from both sides, so a month
   with a budget and no sales still shows.

It is the same shape the planner already uses for two facts at
different grains, stitched on conformed dimensions. Two rules keep it
right:

- **Every dimension the request asks for must be on both sides,** or be
  a join key. Asking for budget by store, when the budget has no store,
  would repeat each region's budget on every store, and is refused.
- **Nothing is regrouped after the join.** The join keys are asked
  for, so the answer is at their grain. For a coarser answer, group
  the dataset in its own recipe and join on the coarser keys: the
  warehouse then computes every measure at that grain, a distinct count
  or a ratio included.

**Limits are the warehouse query's.** A request's limit applies to the
model's part, to bound what the warehouse sends; the merge takes no
limit of its own, and datasets are bounded by their caps. A top-N with a
`filter` blend is therefore the top N of the model, filtered, and can
return fewer than N rows. This is deliberate, to protect the server.

**The cache is reused.** A blend's result is kept by the run of the
model's part, each attached dataset's id and materialization version,
and the joins. The run is the run cache's: `fresh`, the datasource's
`cache_ttl`, an invalidation and the credential decide when the model's
part runs again, and a new run is a new key. Each dataset is found
before the lookup, and a new materialization is a new key, so a match
is the same question over the same data. A project keeps its 64 most
recent blends.

## Only when asked

- A dataset is in no catalog: not in `fields`, `tables` or explore, not
  in sampled values, synonyms or field hit counts, not in what an MCP
  client is told.
- Its columns resolve only under the alias the request declares, and an
  alias that is also a name in the model is refused. A bare term never
  resolves to a dataset column; corrections never suggest across.
- **On `/mcp` the agent can use datasets, when asked.** Picking a
  dataset in conversation ("blend this with my budget") is the user
  picking it. What the agent cannot do is use one without naming it,
  or reach one the application has not let it:
  - which datasets an agent may name is the application's to decide,
    as which projects it may ask about is;
  - datasets are in no catalog or instruction the agent is given; it
    finds them only by calling a dataset tool, and uses one only by
    naming its id in `datasets:`;
  - the dataset tools are off unless `ZSQL_MCP_TOOLS` names them, with
    reading and blending apart from creating and changing, so an
    application can allow the first without the second;
  - every answer a dataset fed names it.

  With the write tools on, an agent can do what the API does: make a
  dataset from a semantic query, add steps, refresh.
- **It holds by construction:** the planner does not depend on data
  prep and cannot see a dataset. A golden test holds it too: a model
  plans byte for byte the same with datasets present and none attached.

## Deleting

**A delete is a hard delete.** Every snapshot, recipe version, edit log
and materialization of the dataset, and its copy in the object-store
backup, are removed, and the cached blends that cite it are marked
dirty. A dataset others read is deleted only after they are named and
the user confirms; they go stale. A project's removal,
`DELETE /projects/{uid}`, removes its datasets.

## Where it is kept

The data directory keeps the layout it has ("What is on disk" in
`docs/operating.md`); datasets are added inside each project:

```
<data-dir>/
  lock
  <org>/<project>/
    <branch>/              a deployed branch, as today
    .datasets/<id>/
      dataset.json         the manifest, written last: the registry entry
      recipe/r<m>.json     every recipe version
      edits.log            the edit log
      sources/<s>/<n>.*    snapshots: rows.arrow + manifest
      v<k>/                materializations: rows.arrow + manifest + lineage
  .cache/results/  .cache/records/
```

- **Nothing that exists moves.** A branch name never starts with a dot,
  and the loader passes over a dot directory that is not a deploy's
  leftover (`crates/server/src/run.rs`), so `.datasets` is never taken
  for a branch nor swept. Removing a branch removes that branch's
  directory only.
- **Renaming an org** stays what it is: the directory renamed with the
  server stopped. The datasets are inside it and go with it.
- **A test holds it:** a `.datasets` directory survives startup, the
  leftover sweep, a deploy and a branch removal.
- **Deleting a project deletes its datasets.** `DELETE /projects/{uid}`
  already removes the project's whole directory, `.datasets` with it.
  Once datasets exist it also calls data prep's own delete for each,
  so that their backup copies in the object store go too and the cached
  blends that cite them are marked dirty.
- The manifests are the registry of datasets.
- Snapshots are kept so that an early step can be changed and the
  recipe run again; how many is a retention setting.
- Only data prep's `store` module knows where datasets are, so moving
  them to a volume of their own later touches nothing else.
- **Backup.** With `ZSQL_OBJECT_STORE` set, each committed recipe
  version, edit log, snapshot and materialization is copied under
  `datasets/<org>/<project>/<id>/` there: a backup, read back only by
  a restore (`POST /datasets/restore`, for the org, bringing back every
  dataset the store has whole and this node does not). What is
  immutable is sent once; the edit log and the manifest every time, the
  manifest last, so a dataset in the store is whole once it is there. A
  failed copy is retried, does not fail the job, and the dataset's
  `backup` says it is not backed up. A delete removes the copy.

Routes are under `/projects/{uid}/datasets`, and under `/orgs/{org}`;
with no org the org is `default`.

```
GET    /projects/{uid}/datasets                         the project's datasets
POST   /projects/{uid}/datasets                         body: a CSV  ?name=&file_name=&delimiter=&quote=&header=&max_bad_rows=
POST   /projects/{uid}/datasets/query                   {"name", "branch", "spec" | "expr", "context", "connection", "wait"}
POST   /projects/{uid}/datasets/{id}/refresh            {"context", "connection", "wait"}: its query again, fresh
GET    /projects/{uid}/datasets/{id}
DELETE /projects/{uid}/datasets/{id}
PUT    /projects/{uid}/datasets/{id}/upload             body: a CSV: a new snapshot, the recipe run over it
GET    /projects/{uid}/datasets/{id}/rows               ?offset=&limit=&version=&sort=col:desc&filter=<formula>
GET    /projects/{uid}/datasets/{id}/lineage            ?version=
GET    /projects/{uid}/datasets/{id}/recipe             ?version=
POST   /projects/{uid}/datasets/{id}/recipe             {"against": m, "roles": {...}, "steps": [...]}
POST   /projects/{uid}/datasets/{id}/preview            {"steps": [...], "upto": n, "limit": n}: over a sample, kept nowhere
GET    /projects/{uid}/datasets/{id}/impact             ?column=: what reads it
GET    /projects/{uid}/datasets/{id}/edits              the edit log, and each edit's state
POST   /projects/{uid}/datasets/{id}/edits              {"against": m, "edits": [{"op": "cell" | "delete" | "add" | "undo", ...}]}
POST   /projects/{uid}/datasets/{id}/confirm            {"against": m}: past a hold
GET    /projects/{uid}/datasets/{id}/sources/{source}   ?snapshot=
```

A dataset can be made only in a project deployed on some branch of
the org.

## Running it

Every materialization, refresh, cascade and preview has a seat in an
admission class of its own (`prep`, `ZSQL_ADMIT_PREP`), a DuckDB memory
limit (`ZSQL_PREP_MEMORY`), a thread count (`ZSQL_PREP_THREADS`) and a
spill directory of its own under `.cache/prep`, removed when it closes:
DuckDB names its spill files alike in every instance, so sessions
sharing one would read and delete each other's. One job runs on a
dataset at a time; a second is told it is busy. The caps are
`ZSQL_DATASET_MAX_ROWS` (a million) and `ZSQL_DATASET_MAX_BYTES`
(256 MB).

What bounds a job, beyond the caps:

- **Five minutes,** all its queries together, after which what is
  running is interrupted and nothing more runs. A request answered at
  its deadline keeps its admission seat until its work has stopped, so
  work past its deadline never piles up past the cap.
- **What the engine hands back is counted as it comes,** rows and Arrow
  bytes, and refused past twice the caps: DuckDB's memory limit does not
  count what has left it.
- **A file** has at most 4,096 columns, fields of at most 1 MB, and 32
  million values in all.
- **A formula's `replace()`** that lengthens is refused when it could
  make a value past 64 KB: nested, each one multiplies the length, and
  DuckDB's memory limit does not hold back a value that size.

A `sql` step's query can still make large values inside the engine
(`repeat()`, `lpad()`); the memory limit does not count those, and what
would bound them is a process of its own with an OS limit. That is left
for when a job runs out of process.

An upload and a recipe change run while the request waits: a dataset
is small, and the deadline is ninety seconds. A refresh from the
warehouse that outlasts its request is answered 202 and waited on
after.

### Crashes, stops and restarts

A dataset is never half made: its manifest is written last, synced to
disk with the files it names before it is renamed into place. What a
crash or a stop does to the jobs running is said on the dataset.

- **Every job leaves a marker** (`job.json` in the dataset's directory)
  while it runs; a query a dataset waits on is kept in the project's
  `.datasets/.jobs/`, never with its security context or a
  `connection`, which are the request's.
- **A job a start finds running died with the process.** It may be
  what killed it, so it is not run again. The dataset keeps the version
  it had and says so in `interrupted`: the job, when it started, when
  the start found it, `"cause": "crash"`, and a reason in words ("the
  server restarted and found this recipe change in flight. It may be
  what stopped the server, so it was not run again. Send it again.").
  The dataset's next job ends it. A cascade passes over an interrupted
  dataset, with a note, rather than run what may be what crashed.
- **A stop gives running jobs ten seconds,** then stops the rest and
  keeps them as *suspended*; the next start marks each `interrupted`,
  with `"cause": "stop"`. **Nothing is resumed:** the application stopped
  the server, and it decides what to send again.
- **A dataset made from a query that never finished** has no manifest
  to carry the word: its id answers as a pending one that failed, with
  `interrupted` beside, for a week.
- **What jobs left half made is swept** before anything is served:
  spill directories, staging directories, files written beside their
  targets, half-kept blends. One server owns a data directory (it holds
  its lock), so nothing swept is anyone's.

## How the code is laid out

One crate, `crates/prep` (`zsql-prep`). Nothing else in 0sql changes to
know about datasets.

| module | what it holds |
|---|---|
| `source` | source kinds; the CSV contract, parsed in Rust |
| `snapshot` | rows that arrived, typed and met with their source's types |
| `schema` | column roles, types and their evolution |
| `recipe` | steps, recipe versions, the edit log, the column graph |
| `formula` | the formula language: parser, allowlist, compiler to DuckDB SQL |
| `compile` | steps to DuckDB SQL |
| `edits` | remaps, overrides by dimension members, orphans and flags |
| `engine` | DuckDB, opened with external access off and its limits |
| `materialize` | running a recipe, previews, pages, the DAG cascade |
| `lineage` | what each materialization was made from; upstream, downstream, impact |
| `store` | `.datasets`, manifests, atomic writes, hard delete, the backup |
| `blend` | attaching a dataset to a request; filter, enrich, their rules |

- **Dependencies point one way.** `zsql-prep` depends on no part of
  the planner or the model: the server runs a semantic source's spec
  through its ordinary run path and hands data prep the rows. Nothing
  that plans a query depends on `zsql-prep`. That is what keeps datasets
  out of automatic query generation.
- **The server holds only routes,** in one module, and the attachment
  header on `/mcp`.

## Later

- **MCP.** Dataset tools on `/mcp`, as "Only when asked" describes:
  read and blend tools, then create and change tools, each off until
  named. Until then datasets are reached only through the HTTP routes.
- **Feeds:** pushed snapshots, then connectors, then incremental syncs.
- **Sharing.**
- **Scheduled refresh,** with a stored, signed context.
- **Job records and crash recovery.**
- **Sending a small key list to the warehouse,** off unless an org turns
  it on.
- **Other formats:** Parquet, Excel, JSON lines.
- **Quotas and retention.**
- **More than one node:** a concern for `docs/clustering.md`.

## Sources, datasets and pipelines (planned)

Planned 2026-11-27, not built; revised the same day after a review and
the user's answers. It replaces "a dataset has one source and one linear
recipe", which makes a result of several inputs a chain of datasets:
each intermediate one to share, version and delete, its lineage split,
and a refresh a cascade between them.

### Who does what

A user who is not an engineer, helped by an AI, gets data from where it
is (a file, the warehouse, a tool) and shapes it into what their
analysis needs. The application in front of 0sql shows them **datasets**
to browse; editing one shows its **pipeline** as a graph, with a preview
of the output below and, on every node, what the last run did there.
The inputs a pipeline reads are found in an explorer when building one,
and are not where a user starts.

The split is the contract:

- **The application owns the pipeline:** it builds the graph, names its
  nodes, keeps its versions, drafts and diffs, places a user's edits in
  it, and decides when it runs: on a click, on a schedule, after a new
  upload, after a stop. It decides who may read what, as today.
- **0sql runs a graph it is given.** It checks it, runs it under the
  context it is sent, keeps the output, and says what each node did. It
  keeps no pipeline of its own, no schedule, and runs nothing it was
  not asked to.

### Three things

- **A source** is where data comes from, and what came: its kind, how it
  is read, and its **snapshots**, each a pull or an upload kept whole.
  A source is not browsed as a dataset; it is what the "add an input"
  explorer lists. It has a name and a description for the explorer.
- **A dataset** is what a user sees, shares and blends: a name and its
  **outputs**, versioned, each made by one run.
- **A pipeline** is the graph a run is sent: nodes from its inputs to
  one output. 0sql keeps a copy of it with each run, as the record of
  what made that output, never as something to run again.

**An upload used as it is creates a source and a dataset over it, and
copies nothing.** The dataset's output version is a pointer to the
source's snapshot, with the roles the dataset gives its columns. One
thing is listed, under one id, to share and blend. A run of a graph
with nodes in it makes the next output version real rows; the id, its
shares and the blends that name it stay.

```
<org>/<project>/.datasets/
  .sources/src_7f3a…/        a source
    source.json                kind, name, read options, every version of them
    snapshots/<n>/             rows.arrow + manifest (columns, types, parse report,
                               the context's fingerprint for a query)
  ds_91fc…/                  a dataset
    dataset.json               name, roles, live output version
    v<k>/                      an output: rows.arrow, or a pointer to a snapshot;
                               manifest, lineage, run record, the graph it ran
  .runs/<run id>.json        a run in flight, or answered, for a retry to find
```

### Sources

| kind | a snapshot is | taken when |
|---|---|---|
| `upload` | the parsed file | each upload to it |
| `query` | a spec run on a branch, under the run's context | a run pulls it |
| `push` | rows an application or an agent posts, with where they came from | each post |
| `tool` (later) | what an MCP tool or a connector returned, called by 0sql | a run pulls it |

- **Reading is the source's:** the CSV contract, the parse report,
  inferred types, and the type rule on each new snapshot ("Types
  evolve"). A wrong inference is fixed on the source (`types`), as a new
  version of its read options; a graph may read a source with types of
  its own instead, for itself only.
- **A new snapshot runs nothing.** A new upload or a pull adds a
  snapshot; a dataset reading the source changes on its next run, when
  the application sends one.
- **A query source keeps its spec, never a context.** A run pulls it
  under the context the run was sent, and the snapshot records that
  context's fingerprint. B's pipeline reading A's query source reads it
  as B. A run that reads the latest snapshot rather than pulling reads
  it only if the fingerprint is the run's own; otherwise it pulls.
- **`push` comes before `tool`:** an agent calls a tool on its side and
  posts the rows, so 0sql calls out to nothing and holds no third
  party's credential. `tool` waits for a reason to call out.
- **What came is kept as it came:** a run reads a snapshot, never the
  live source, so a run can be done again on the same data, and a tool
  is not called again to repeat one.

### A run

A request is the graph, the output dataset it makes, the context, and a
**run id** the application chooses. A run id sent again answers what the
first one did, in flight or done, so a retry makes nothing twice. One run
per output dataset at a time; another is `409 Busy`.

The graph is nodes, each with the application's id, which 0sql never
renames and keys everything it says by:

| node | reads | makes |
|---|---|---|
| `source` | a source, by id | a snapshot, as the input says |
| `dataset` | another dataset's output, by id | that output, as the input says |
| `steps` | one node | the steps of today's recipe, in order |
| `join` | two nodes | today's join, between nodes rather than datasets; the only way to join |
| `append` | two or more nodes | today's append |
| `sql` | one or more nodes, each under an alias the node names | a query, a black box as today's `sql` step |
| `edits` | one node | the edits it carries, inline |
| `output` | one node | the dataset's output |

- **An input says what it reads:** `latest` (the newest snapshot or
  version at run time; the default), a snapshot or version id, or for a
  source `pull` (pull first) or `max_age` (pull when the newest is older
  than this, else read it and say how old it is). The run record says
  what each input read.
- **First it is checked, whole,** before anything is pulled or written:
  every query input plans under the context; every source and dataset
  named exists; every node compiles, with the columns and types it
  makes; the output contract holds on the schema. A run refused says,
  by node id, every reason, typed, and where it can tell, a fix: a
  column gone and one new with most of its values the same is
  suggested by name, type and overlap. Which datasets a context may
  read is the application's, so 0sql checks only that they exist.
- **A dry run** is that check and nothing more, plus what a run would
  pull and read: "pulls revenue (last pulled 3 hours ago, as another
  user, so again), reads the budget file of Monday, runs 6 nodes".
- **A preview** is the same request with a node to stop at, over a
  sample or the whole input, keeping nothing. A join previewed over a
  sample says its counts are of a sample, since rows unmatched there
  may match in the whole.
- **One run** compiles the graph to views in one engine session, as a
  recipe is today, and keeps one output version, whole or not at all,
  with one lineage: each snapshot and dataset version read.
- **The rules carry over:** a column a node reads that is gone fails the
  run, naming the node; the caps; the engine sandbox.

### Edits are a node

A user's cell edits go in an `edits` node the application places in the
graph, carried inline in it:

```
input1, input2 → join → edits → filter → blend with a query
```

An edit names its rows by value, the key columns of the rows at its node
("where `store_id` = 17, set `region` to West"), not by position, so it
holds when what is upstream changes. An `edits` node names its key; a key
that is not unique where the node is fails the run. The run record says
of each edit whether it applied, found no row (orphaned), or found its
value already changed upstream (flagged). Where to put the node, and
moving it, is the application's.

This replaces today's edit log, which 0sql keeps and places by step
position.

### An output contract

The `output` node may declare its key (or grain) and checks: `unique`
and `not_null` first, a row count within a share of the last run's
later. Each check either holds the run, keeping the last output, or
lets it through with a warning. The application, or an AI for it,
writes them; every run checks them.

### Every run says what it did

A run keeps, with the output version, a record per node, which the
application shows on the graph and an AI reads to explain a run:

| node | recorded |
|---|---|
| `source`, `dataset` | which snapshot or version, pulled or reused and why, rows and columns, age |
| `steps`, `sql` | rows in and out; columns added, removed, retyped; values changed per step ("trimmed 412 values", "filled 37 blanks"); time; warnings |
| `join`, `append` | rows matched and unmatched each side; a side refused for repeating keys; columns met by name |
| `edits` | each edit's state: applied, orphaned, flagged |
| `output` | rows, bytes, the version made, each check's result |

Each node also has a **one-line summary in words**, written from those
numbers by the server, not by a model: "Joined revenue: 1,204 of 1,210
rows matched; 6 stores have no revenue." It costs nothing, says the same
every time, and is true. Beside it is a **profile** of what the node
made: per column its nulls, distinct values, minimum and maximum. The
summary is for a person; the profile is what an AI reasons from.

Two runs' records, node by node, say why a number changed; the
application diffs them, as it diffs its graphs.

### Crashes and stops

A run is a job like any other ("Crashes, stops and restarts"), with one
change: **0sql resumes nothing.** A run a start finds is interrupted,
cause `crash` or cause `stop`, and the application, which stopped the
server or saw it crash, decides what to send again. (Resuming at start,
`ZSQL_RESUME_JOBS` and a query's context kept on disk while it ran are
gone already.)

### The API, in outline

```
POST   …/sources?kind=upload           a file → a source (and with dataset=true, a dataset over it)
POST   …/sources/{id}/snapshots        a new file, or pushed rows
PATCH  …/sources/{id}                  name, description, read options (a new version of them)
GET    …/sources                       the explorer: kind, name, columns, last snapshot
POST   …/runs                          a graph, its output dataset, a context, a run id
POST   …/runs?check=true               a dry run
POST   …/preview                       a graph and a node to stop at
GET    …/runs/{id}                     a run, in flight or done, node by node
GET    …/datasets/{id}/v/{k}           an output version, with its run record and graph
```

Datasets keep their routes, rows, lineage, blends and deletion; a blend
reads a dataset's output whatever made it. Every answer that reads
inputs (a run, a preview, a dry run) carries `reads`: each source and
dataset, by kind and id, so the application checks one list against who
is asking.

### Deleting, and retention

- Deleting a dataset deletes its outputs and run records. Its sources
  stay.
- Which sources no pipeline reads is the application's to know, since it
  keeps the pipelines; 0sql deletes a source when asked, and a dataset
  that read it fails its next run, saying so.
- Snapshots kept per source are capped; a snapshot a dataset's live
  output points to, or read, is never the one let go.

### From today's datasets

Today's dataset is a pipeline already, of one shape: a `source` node, a
`steps` node with its recipe, an `edits` node, and the output. Its
source becomes a source; its recipe, a graph the application sends;
its `join` and `append` steps, `dataset` inputs; its edit log, edits
inline by key; its cascade goes, since nothing runs unasked. Nothing is
in production, so this is a rewrite of the store's layout, not a
migration.

### Settled, and open

Settled with the user: datasets are what is browsed; sources are
first-class and found in an explorer; an upload used as it is is one
dataset over one source, copying nothing; a pipeline has one output;
the application owns the graph, its ids, its versions, its edits and
when it runs, and sends the context a run is under; 0sql checks and
runs it, stores no context, resumes nothing and runs nothing unasked.

Open: whether a source keeps the original file beside the parsed rows,
to read it again with other options; incremental pulls (append past a
cursor, or upsert by a key), left out of the first version, where a
pull replaces; a pipeline with more than one output.

Later: linking a dataset's columns to the model's fields; suggestions
drawn from a run's own data ("37 blanks in owner, fill down?"); row
diffs between output versions; `tool` sources.

## Phases

- [x] **1. The core, fed by an upload.** `.datasets` under each project
  and the test that pins it; The crate,
  the CSV contract, roles and types, snapshots, recipes and
  materializations, the caps, lineage records, hard delete, the engine
  with external access off.
- [x] **2. Steps, formulas and the grid.** The formula language and its
  compiler; the first steps; the column graph, held materializations;
  pages, previews, recipe versions.
- [x] **3. Edits.** Investigate the open questions first; then remaps,
  measure overrides, the edit log, orphans and flags.
- [x] **4. A semantic query as a source,** refreshed fresh, by request.
- [x] **5. Combining datasets.** Joins and appends, the DAG and its
  cascade.
- [x] **6. Blends.** Attachment in the request, the isolation golden
  test, `filter`, `enrich` as a look-up and as budget against actual,
  every model key asked for, cross joins, the cache. MCP went to "Later".
- [x] **7. The object-store backup** and restore.
- [ ] **8. Feeds and sharing,** designed then.
- [ ] **9. Sources, datasets and pipelines** (planned above): resume
  dropped (done); sources and their snapshots, `push`; a run of a graph
  sent, its checks, dry run and preview; zero-copy outputs; edits inline
  by key; the output contract; the per-node run record, summaries and
  profiles; the API; today's datasets rewritten to it.

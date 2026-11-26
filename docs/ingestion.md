# Data prep: datasets beside the model

A plan, started 2026-11-26, revised after a review the same day. High
level only; nothing here is built or scheduled. One node only; more than
one is out of scope.

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
 upload         immutable      replayed on every       Parquet, immutable,
 semantic query                new snapshot            records its lineage
 dataset
 feed (later)
```

- A **source** brings rows; each time it does, they are kept as a
  **snapshot**.
- The **recipe** is an ordered list of **steps** that shape them.
- A **materialization** is the recipe run over the latest snapshots,
  kept as Parquet: what the grid shows and what is queried.

Editing in the grid adds to the recipe; stored rows are never changed.
A sync brings a new snapshot and runs the same recipe over it, which is
how every edit is applied again.

The warehouse stores nothing. A semantic source is the one thing that
reads it, and it only reads.

**Datasets are small.** A dataset is capped in rows and bytes, at import
and at every materialization, so that the server's own engine never
holds more than it was sized for. A source or a step that would pass
the cap fails, saying so.

## Who owns it

- The application sends an **owner reference** with every data prep
  request, an opaque string it chooses, trusted as `context` is. A
  request without one is refused; there is no shared empty owner.
- A dataset belongs to **(org, project, owner)**; under any other its id
  answers 404. The reference is stored and logged only as a hash.
- **Securing a dataset is its creator's.** The server keeps owners apart
  and does nothing more.
- **Who may create one is decided in front.** Datasets may be for
  developers only; that is the application's choice, by the routes it
  forwards. `ZSQL_DATASETS=off` turns the feature off as a whole.
- **Per project, not per branch.** A deploy never takes a dataset away.

## Sources

| source | a snapshot is | taken when |
|---|---|---|
| upload | the parsed file | each upload, the first and every replace |
| semantic query | the result of a spec run on a branch | the dataset is refreshed |
| dataset | another dataset's materialization | that dataset has a new one |
| feed (later) | what an upstream system pushed or a connector pulled | each sync |

Each ends in a snapshot as Parquet with a manifest, so the recipe never
knows which kind fed it.

**A CSV upload** says, or has detected and recorded: encoding (UTF-8,
with a BOM accepted), delimiter, quote, whether there is a header. Types
are inferred, and a column that looks like an identifier (leading zeros,
postcodes) stays text. Column names are made unique and case-distinct;
an empty name becomes `column_<n>`. A row that does not parse is counted
and the first few are shown; past a threshold the import fails rather
than quietly dropping data. Every choice is in the manifest.

**A semantic source** runs its spec under the context sent with the
refresh, row policies compiled in. The snapshot shows what that context
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
reads and writes and compiles to SQL for DuckDB. No user SQL, anywhere.

- **Columns:** rename, retype, remove, reorder, a formula column.
- **Rows:** filter, sort, remove duplicates, fill empty values.
- **Values:** find and replace, remap a value, trim, change case.
- **Shape:** group and aggregate, append.
- **Combine:** join or look up another dataset.
- **Edits:** see "Editing a cell".

**Formulas have their own language.** A formula column or a filter is
written in a small expression language of data prep's own: column
references, literals, operators, and an **allowlist** of functions
(text, numbers, dates, conditionals), compiled to DuckDB's SQL by data
prep's compiler. It does not reuse `calc(...)` from the query
shorthand, which carries raw SQL. DuckDB is opened with external access
off, so no formula and no step can read a file, a URL or another
dataset's directory.

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

**A column the user deletes in the grid** is a remove step, checked by
the same graph first. With no dependents it is removed; with some, the
removal is refused, naming them, with the choice to remove them too.
The rows stay in the snapshot, so undo brings the column back.

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

**Still to investigate before edits are built:** a dimension edit meant
for one row only, not every row with that value; measure edits on a
dataset whose dimensions do not identify rows, which may need the user
to mark a key or the edit to apply to all matching rows; and edits on
a result past a join or an append.

**Edits are a log, not a recipe version each.** An edit appends to the
recipe's edit log; pasting five thousand cells is one append. The log is
folded into the recipe at its position when the recipe runs.

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
- Reading an input is checked as reading it directly would be.

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
  - join: targets_list
    to: model
    on: [{ left: customer.id, right: targets_list.customer_id }]
    kind: filter
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
  collects the model columns every join needs, asks the ordinary
  planner for them at those dimensions, and does every join after, in
  DuckDB.
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
- **The model's measures are only regrouped after the join if they add
  up.** Asking for a coarser grain than the join keys works for
  revenue; a distinct count or a ratio there is refused. For example,
  two Gold customers, A buying p1 and p2 and B buying p2: per-customer
  distinct products are 2 and 1, adding gives 3, the truth is 2. The
  refusal says what works: a dataset made from a semantic query at the
  grain needed, grouped in its own recipe.

**Limits are the warehouse query's.** A request's limit applies to the
model's part, to bound what the warehouse sends; the merge takes no
limit of its own, and datasets are bounded by their caps. A top-N with a
`filter` blend is therefore the top N of the model, filtered, and can
return fewer than N rows. This is deliberate, to protect the server.

**The cache is reused.** A blend's result is cached by the model's SQL,
each attached dataset's id and materialization version, and the joins.
A dataset can be attached only by its owner, the owner check comes
before the lookup, and a new materialization is a new key, so a match is
the same question over the same data.

## Only when asked

- A dataset is in no catalog: not in `fields`, `tables` or explore, not
  in sampled values, synonyms or field hit counts, not in what an MCP
  client is told.
- Its columns resolve only under the alias the request declares, and an
  alias that is also a name in the model is refused. A bare term never
  resolves to a dataset column; corrections never suggest across.
- **On `/mcp` the agent can use datasets, when asked.** Picking a
  dataset in conversation ("blend this with my budget") is the user
  picking it. What the agent cannot do is choose whose datasets, or use
  one without naming it:
  - the owner comes only from a header the application sets on `/mcp`,
    never from a tool's arguments, so the agent reaches only the
    datasets of the user it acts for;
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
the user confirms; they go stale. Also: delete every dataset of one
owner, for an application removing a user; and a project's removal,
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
      recipe/r<m>.json     every recipe version
      edits.log            the edit log
      sources/<s>/         snapshots, Parquet + manifest
      v<k>/                materializations, Parquet + manifest + lineage
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
  `datasets/` there: a backup, read back only by a restore command. A
  failed copy is retried, does not fail the job, and the dataset says it
  is not backed up.

Routes are under `/projects/{uid}/datasets`, and under `/orgs/{org}`;
with no org the org is `default`.

## Running it

Every materialization, refresh, cascade and preview is a job, answered
202 with an id, with a seat in an admission class of its own, a DuckDB
memory limit and a spill directory. How jobs are recorded and recovered
after a crash is left until the jobs exist.

## How the code is laid out

One crate, `crates/prep` (`zsql-prep`). Nothing else in 0sql changes to
know about datasets.

| module | what it holds |
|---|---|
| `source` | source kinds, each producing a snapshot; CSV parsing |
| `schema` | column roles, types and their evolution |
| `recipe` | steps, recipe versions, the edit log, the column graph |
| `formula` | the formula language: parser, allowlist, compiler to DuckDB SQL |
| `compile` | steps to DuckDB SQL |
| `edits` | remaps, overrides by dimension members, orphans and flags |
| `engine` | DuckDB, opened with external access off and its limits |
| `materialize` | running a recipe, previews, pages, the DAG cascade |
| `lineage` | what each materialization was made from; upstream, downstream, impact |
| `store` | `zsql-datasets`, manifests, hard delete, the backup |
| `blend` | attaching a dataset to a request; filter, enrich, their rules |

- **Dependencies point one way.** `zsql-prep` uses the planner, the
  dialects and `zsql-exec`'s run path; none of them uses it. That is
  what keeps datasets out of automatic query generation.
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

## Phases

- [ ] **1. The core, fed by an upload.** `.datasets` under each project
  and the test that pins it; The crate, the owner reference,
  the CSV contract, roles and types, snapshots, recipes and
  materializations, the caps, lineage records, hard delete, the engine
  with external access off.
- [ ] **2. Steps, formulas and the grid.** The formula language and its
  compiler; the first steps; the column graph, held materializations;
  pages, previews, recipe versions.
- [ ] **3. Edits.** Investigate the open questions first; then remaps,
  measure overrides, the edit log, orphans and flags.
- [ ] **4. A semantic query as a source,** refreshed fresh, by request.
- [ ] **5. Combining datasets.** Joins and appends, the DAG and its
  cascade.
- [ ] **6. Blends.** Attachment in the spec, the owner header on `/mcp`,
  the isolation golden test, `filter`, `enrich` with budget against
  actual, the cache.
- [ ] **7. The object-store backup** and restore.
- [ ] **8. Feeds and sharing,** designed then.

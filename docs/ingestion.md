# Data prep: sources, datasets and pipelines

A plan, started 2026-11-26 and revised after reviews that day and the
next, and built. Phases 1 to 7 made a dataset a recipe over one source;
phase 9 replaced that with a graph the application keeps and sends, and
0sql runs. One node only; more than one is out of scope.

## What it is for

The model describes the warehouse as its developers built it. A
**dataset** is the data that is not there: a list a user uploads, a
slice of the model they want to reshape, budgets kept in a spreadsheet,
rows an agent got from a tool. **Data prep** is small-scale, last-mile
ETL over such data: bring rows in, shape them step by step, combine
them, and use the result on its own or blended with the model.

Data prep is **independent of the semantic model.** It has its own
crate, its own formula language and its own engine. It reads the model
only through the ordinary planner, as any caller does, and the model
never reads it.

```
 sources ──► snapshots ─┐
 upload        immutable ├──► a run of a graph ──► output v<k> ──► page, blend, read by another graph
 push                    │    the application's,     immutable; records its run,
 query (pulled by a run) │    sent with each run     graph and lineage
 a dataset's output ─────┘
```

- A **source** brings rows; each time it does, they are kept as a
  **snapshot**.
- A dataset's **pipeline** is a graph of nodes from its inputs to one
  output. The application keeps it, and sends it with each run.
- An **output** is what a run made, kept as an Arrow IPC file, as
  results are: what the grid shows and what is queried. A source read
  as it is, to an output, is no copy: the output points at the snapshot.

Editing in the grid adds to the graph, in an `edits` node; stored rows
are never changed. A new snapshot runs nothing: the application runs
the graph again, which is how every step and edit is applied again.

The warehouse stores nothing. A query source is the one thing that
reads it, and it only reads.

**Datasets are small.** A snapshot and an output are capped in rows and
bytes, so that the server's own engine never holds more than it was
sized for. A source or a node that would pass the cap fails, saying so.

## Who may use it

- **The server keeps no user.** A source or a dataset belongs to an org
  and a project and is addressed by its id, as a deployed branch or a
  result is. Who may read, change or share which is the application's
  to decide, with its own records of who made what; the server answers
  whoever reaches it, as it does for everything else. Every answer that
  reads inputs (a run, a dry run, a preview) carries `reads`, each
  source and dataset by kind and id, for the application to check one
  list against who is asking.
- **Sharing is the application's.** A user shown a chart built on a
  dataset reads that dataset because the application lets them; the
  server is not asked, and there is nothing to grant. (An owner header
  scoping datasets per user was built first, and dropped on 2026-11-26
  for this: it duplicated the application's access control and fitted
  no sharing model.)
- **A query source is read as a context.** A run pulls it under the
  context the run was sent with, and its snapshot records that
  context's fingerprint; a later run reads a snapshot only if it was
  taken as its own context, and otherwise pulls. A dataset made from
  one holds that context's view, whoever the application then lets read
  the dataset; the application should say so before such a dataset is
  shared. The server never keeps a context.
- **Who may create one is decided in front** too: by the routes the
  application forwards. `ZSQL_DATASETS=off` turns the feature off as a
  whole.
- **Per project, not per branch.** A deploy never takes one away.

## Sources, datasets and pipelines

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

A dataset reads another dataset's output with a `dataset` node: its
live version, or the version it names.

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
- **A source keeps its 20 newest snapshots,** and every one an output
  points to, or a live output read.

**A CSV upload** says, or has detected and recorded: encoding (UTF-8,
with a BOM accepted), delimiter, quote, whether there is a header. Types
are inferred, and a column that looks like an identifier (leading zeros,
postcodes) stays text. Column names are made unique and case-distinct;
an empty name becomes `column_<n>`. A row that does not parse is counted
and the first few are shown; past a threshold (one row in a hundred, or
`max_bad_rows`) the import fails rather than quietly dropping data.
Every choice is in the manifest. The file is parsed by data prep in
Rust, not by the engine.

**Pushed rows** are a JSON array of objects, column to value, and an
`origin` kept as the pusher says it; a column is every name any row has,
in the order first seen, its type inferred as a file's is.


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
| `steps` | one node | its steps, in order ("Steps") |
| `join` | two nodes | the right's columns beside the left's, by keys, `left` or `inner`; the only way to join |
| `append` | two or more nodes | their rows one after another, columns met by name |
| `sql` | one or more nodes, each under an alias the node names | a query, a black box as the `sql` step is |
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
- **One run** makes each node a table in one engine session, in an
  order where each comes after what it reads, and keeps one output
  version, whole or not at all, with one lineage: each snapshot and
  dataset version read.
- **A column a node reads that is not there refuses the run,** naming
  the node, the step and the column, with every later step that reads
  what it would have made. The output stays as it was. Where it can
  tell, it suggests the column meant: one named the same but for case or
  punctuation, or a letter or two apart, and first, for a source, a
  column new in its snapshot that holds most of the values the gone one
  had in the snapshot before ("'region' is gone since snapshot 3, 'Area'
  is new, and 98% of the values 'region' had are in it"). A column new
  upstream is added after the others, and the snapshot says so.

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

Edits apply in the order the node lists them: a row deleted and then
its value remapped stays deleted; a remap of a remapped value chains
(`NY` to `New York` to `NYC`); a row added and then deleted is gone. An
edit that reads a column the node's input does not have refuses the
run, as a step's does.

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

## Columns have roles

Each column of a dataset is a **dimension** or a **measure**. Inferred at
import (text, dates, booleans and identifier-like columns are
dimensions; other numbers are measures) and given by the graph's output
node (`roles`). A query source's columns take their roles from the
model. Roles decide how a dataset blends, and what a grid offers for an
edit: a dimension cell is usually a remap, a measure cell a set.

## Types evolve, never truncate

When a new snapshot, a join or an append meets a column of another type:

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

What a `steps` node does, in order: a **closed set** of declarative operations. Each declares the columns it
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
  count_distinct), columns made rows (`unpivot`).

Joining, appending and a user's edits are nodes of the graph, not
steps ("A run").

A step is JSON with an `op`: `{"op": "remap_val", "column": "region",
"values": [{"from": "NY", "to": "New York"}]}`. A step with any other
`op`, or a field its op does not have, is refused. A `retype_column` keeps a
value only if it converts: text read as a number or a date (with a
strptime format when it is not ISO), a number narrowed only when it
comes back the same; the run fails naming the node, the step, the
column, both types and the first values that do not.

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
  into a DuckDB session of its own. The other nodes and steps are not
  there. A `sql` **node** is the same query over several inputs, each
  copied in under the alias the node gives it (`{"kind": "sql",
  "inputs": {"sales": "a", "stores": "b"}, "query": "SELECT ... FROM
  sales JOIN stores USING (store)"}`), and reads those and nothing else.
- **It is one `SELECT`, checked by DuckDB's own parser** before it
  runs, so no quoting hides anything from the check. It reads `input`
  and the tables its own `WITH` makes (recursive ones too), and no
  other table, schema or catalog; its table functions are `range`,
  `generate_series` and `unnest`. A trailing `;` is fine. DuckDB's
  JSON extension is compiled in for the parse.
- **It is the same every run.** The clock and randomness (`now()`,
  `current_date`, `random()`, `uuid()` and the like) are refused; a
  formula's `today()` is the one clock a run has. So is what runs
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
  that is not there refuses the run as any step's would, naming the
  node, the step and the column. Its lineage is coarse: each column it makes is
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
output itself and hands the engine Arrow batches. No formula
and no step can read a file, a URL or a directory of data prep's.

### Columns are a dependency graph

Every step and node declares what it reads, so each column of an output
has an **ancestry**, back through the nodes to source columns and model
fields, which the lineage keeps. A column that is not there where a node
reads it refuses the run ("A run"); nothing is held, and nothing
guesses. A graph is checked whole before anything runs, so the dry run
says what a change would break before the application saves it.

## The grid

The grid is the application's. The server answers what a grid asks: a
page of an output, or of a snapshot, sorted and filtered for viewing; a
preview of any node; a run of the graph the application saves; and the
record of each run, node by node.

## The API

Routes are under `/projects/{uid}`, and under `/orgs/{org}`; with no org
the org is `default`. A source or a dataset can be made only in a
project deployed on some branch of the org.

```
GET    /projects/{uid}/sources                          the explorer: kind, name, columns, newest snapshot
POST   /projects/{uid}/sources                          ?kind=upload (a CSV) | push ({"rows", "origin"}) | query ({"branch", "spec" | "expr"})
                                                        &name=&description=&dataset=true&file_name=&delimiter=&quote=&header=&max_bad_rows=
GET    /projects/{uid}/sources/{id}
PATCH  /projects/{uid}/sources/{id}                     {"name", "description", "types": {"column": "text"}}
DELETE /projects/{uid}/sources/{id}
GET    /projects/{uid}/sources/{id}/snapshots           the snapshots kept, the newest first
POST   /projects/{uid}/sources/{id}/snapshots           a CSV (upload), or {"rows", "origin"} (push); runs nothing
GET    /projects/{uid}/sources/{id}/snapshots/{n}
GET    /projects/{uid}/sources/{id}/rows                ?snapshot=&offset=&limit=&sort=&filter=
GET    /projects/{uid}/datasets                         what is browsed
POST   /projects/{uid}/datasets                         {"name", "description", "source"}: with a source, its newest snapshot as it is
GET    /projects/{uid}/datasets/{id}
DELETE /projects/{uid}/datasets/{id}
GET    /projects/{uid}/datasets/{id}/rows               ?offset=&limit=&version=&sort=col:desc&filter=<formula>
GET    /projects/{uid}/datasets/{id}/lineage            ?version=
GET    /projects/{uid}/datasets/{id}/versions/{k}       its manifest, lineage, run record and graph
POST   /projects/{uid}/runs                             {"id", "dataset", "graph", "context", "connection", "wait"}
POST   /projects/{uid}/runs?check=true                  {"graph", "context"}: a dry run
GET    /projects/{uid}/runs/{id}
POST   /projects/{uid}/preview                          {"graph", "node", "whole", "limit", "context", "connection", "wait"}
POST   /datasets/restore                                every source and dataset of the org the object store has and this node does not
POST   /projects/{uid}/branches/{branch}/blend
```

- **A run** answers its record: 200 when it is over, whatever it came
  to; 202 while it pulls or runs, past its `wait` (30 seconds unless it
  says, at most 300), and `GET /runs/{id}` reads it after. A graph
  refused before it runs is 422, class `Refused`, with `problems`, each
  of its node; nothing is recorded, since nothing ran. A run id sent
  with another graph, dataset or context is 409; a dataset a run is
  making answers another 409.
- **A preview** answers its node's columns and rows, the record of each
  node it ran, its problems, `reads` and `inputs`. A query source it
  reads with no snapshot of its context is pulled first, joining a run
  of that query already going or kept; past its `wait` (10 seconds) it
  answers 202, and the same request again is answered when the
  warehouse is done.

## Lineage

Every output records what made it:

- the run that made it, and the hash of the graph, which is kept beside
  it as `graph.json`, with the run's record as `run.json`;
- for each snapshot read, by node: the source, the snapshot, the version
  of how it was read, its file name, hash and parse options, or what a
  push said of its origin;
- for each dataset read, by node: its id and output version;
- for each query snapshot: the branch, the deploy, the spec, the planned
  SQL's hash, a fingerprint of the context, and the model field of each
  column (not the run's result id, which expires with the cache);
- the column roles;
- the versions of the server, the compiler and DuckDB;
- the time the run was pinned to, so `today()` in a formula means one
  thing for the whole run;
- what the run let through: a check that warns, edits orphaned or
  flagged.

And each column's ancestry through the nodes to source columns.

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
  as in a join node.

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
  the dataset in its own pipeline and join on the coarser keys: the
  warehouse then computes every measure at that grain, a distinct count
  or a ratio included.

**Limits are the warehouse query's.** A request's limit applies to the
model's part, to bound what the warehouse sends; the merge takes no
limit of its own, and datasets are bounded by their caps. A top-N with a
`filter` blend is therefore the top N of the model, filtered, and can
return fewer than N rows. This is deliberate, to protect the server.

**The cache is reused.** A blend's result is kept by the run of the
model's part, each attached dataset's id and output version,
and the joins. The run is the run cache's: `fresh`, the datasource's
`cache_ttl`, an invalidation and the credential decide when the model's
part runs again, and a new run is a new key. Each dataset is found
before the lookup, and a new output is a new key, so a match
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

  With the write tools on, an agent can do what the API does: push
  rows, make a source, send a graph to run.
- **It holds by construction:** the planner does not depend on data
  prep and cannot see a dataset. A golden test holds it too: a model
  plans byte for byte the same with datasets present and none attached.

## Deleting

**A delete is a hard delete.** A dataset's every output and run record
of it, and its copy in the object-store backup, are removed, and the
kept blends that cite it go. Its sources stay: other pipelines may read
them, and which do is the application's to know, since it keeps the
pipelines. A source's delete removes every snapshot of it; a dataset
output that is one of its snapshots as it is is given those rows first,
so it keeps answering, and a pipeline that reads it fails its next run,
saying so. A project's removal, `DELETE /projects/{uid}`, removes its
sources and datasets.

## Where it is kept

The data directory keeps the layout it has ("What is on disk" in
`docs/operating.md`); data prep's is added inside each project:

```
<data-dir>/
  lock
  <org>/<project>/
    <branch>/                        a deployed branch, as today
    .datasets/
      .sources/<src>/source.json     a source: kind, name, how it is read, every version of that
      .sources/<src>/snapshots/<n>/  rows.arrow + manifest
      <ds>/dataset.json              a dataset's manifest, written last
      <ds>/v<k>/                     an output: rows.arrow, or a pointer to a snapshot;
                                     manifest, lineage, run.json, graph.json
      .runs/<run>.json               a run's record, in flight or done; the newest 1,000 kept
  .cache/results/  .cache/records/  .cache/prep/  .cache/blends/
```

- **Nothing that exists moves.** A branch name never starts with a dot,
  and the loader passes over a dot directory that is not a deploy's
  leftover (`crates/server/src/run.rs`), so `.datasets` is never taken
  for a branch nor swept. Removing a branch removes that branch's
  directory only.
- **Renaming an org** stays what it is: the directory renamed with the
  server stopped. What is inside goes with it.
- **A test holds it:** a `.datasets` directory survives startup, the
  leftover sweep, a deploy and a branch removal.
- Only data prep's `store` module knows where things are, so moving
  them to a volume of their own later touches nothing else.
- **Backup.** With `ZSQL_OBJECT_STORE` set, each dataset is copied under
  `datasets/<org>/<project>/<id>/` there and each source under
  `datasets/<org>/<project>/sources/<id>/`: a backup, read back only by
  a restore (`POST /datasets/restore`, for the org, bringing back every
  source and dataset the store has whole and this node does not, the
  sources first). What is immutable is sent once; the manifest every
  time, and last, so one in the store is whole once it is there. A
  failed copy is retried, does not fail the job, and the manifest's
  `backup` says it is not backed up. A delete removes the copy.

## Running it

Every run, upload, pull and preview has a seat in an admission class of
its own (`prep`, `ZSQL_ADMIT_PREP`), a DuckDB memory
limit (`ZSQL_PREP_MEMORY`), a thread count (`ZSQL_PREP_THREADS`) and a
spill directory of its own under `.cache/prep`, removed when it closes:
DuckDB names its spill files alike in every instance, so sessions
sharing one would read and delete each other's. One run makes a
dataset at a time, and one job takes a snapshot of a source at a time; a
second is told it is busy. The caps are
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

A `sql` step's or node's query can still make large values inside the engine
(`repeat()`, `lpad()`); the memory limit does not count those, and what
would bound them is a process of its own with an OS limit. That is left
for when a job runs out of process.

An upload runs while the request waits: a file is small, and the
deadline is ninety seconds. A run waits for its pulls and its graph as
long as its `wait`, and goes on after it is answered.

### Crashes, stops and restarts

Nothing is ever half made: a manifest is written last, synced to disk
with the files it names before it is renamed into place. What a crash
or a stop does to the jobs running is said on what they worked on.

- **Every job leaves a marker** while it runs: a snapshot being taken,
  `job.json` in its source's directory; a delete, one in the dataset's;
  a run, its record in `.runs/`, saying `running`. Never a context or a
  `connection`: they are the request's.
- **A job a start finds running died with the process.** It may be
  what killed it, so it is not run again. What it worked on keeps what
  it had and says so in `interrupted`: the job, when it started, when
  the start found it, `"cause": "crash"`, and a reason in words ("the
  server restarted and found this run in flight. It may be what stopped
  the server, so it was not run again. Send it again."). A run's record
  says `interrupted`, and so does its dataset's `last_run`. The next job
  on it ends it.
- **A stop gives running jobs ten seconds,** then stops the rest and
  keeps them as *suspended*; the next start marks each `interrupted`,
  with `"cause": "stop"`. **Nothing is resumed:** the application stopped
  the server, and it decides what to send again. A run sent again under
  its id answers its interrupted record; a new id runs it.
- **What jobs left half made is swept** before anything is served:
  spill directories, staging directories, files written beside their
  targets, half-kept blends. One server owns a data directory (it holds
  its lock), so nothing swept is anyone's.

## How the code is laid out

One crate, `crates/prep` (`zsql-prep`). Nothing else in 0sql changes to
know about it.

| module | what it holds |
|---|---|
| `source` | source kinds; the CSV contract, parsed in Rust |
| `snapshot` | rows that arrived, from a file, a push or a pull, typed and met with their source's types |
| `schema` | column roles, types and their evolution |
| `graph` | a pipeline's nodes, its shape checked, and problems |
| `steps` | the steps of a `steps` node |
| `formula` | the formula language: parser, allowlist, compiler to DuckDB SQL |
| `compile` | steps, joins and appends to DuckDB SQL; the column graph |
| `sql` | the `sql` step and node: one query, vetted by DuckDB's parser |
| `pipeline` | a graph compiled against its inputs' columns, and run node by node |
| `record` | a run's record: per node numbers, summaries in words, profiles |
| `runs` | a run: inputs resolved, pulls asked for, checks, the output kept; dry runs and previews |
| `sources` | sources and their snapshots, retention |
| `edits` | edits by value: remaps, sets, deletes, adds, and how each fared |
| `engine` | DuckDB, opened with external access off and its limits |
| `lineage` | what each output was made from |
| `store` | `.datasets`, manifests, atomic writes, hard delete |
| `recovery` | markers, interruptions, the sweep at start |
| `backup` | the object store's copy, and its restore |
| `blend` | attaching a dataset to a request; filter, enrich, their rules |

- **Dependencies point one way.** `zsql-prep` depends on no part of
  the planner or the model: the server runs a query source's spec
  through its ordinary run path and hands data prep the rows. Nothing
  that plans a query depends on `zsql-prep`. That is what keeps datasets
  out of automatic query generation.
- **The server holds only routes,** in one module (`datasets.rs`), and
  the pulls a run waits on.

## Later

- **MCP.** Dataset tools on `/mcp`, as "Only when asked" describes:
  read and blend tools, then tools that send graphs, each off until
  named. Until then datasets are reached only through the HTTP routes.
- **`tool` sources,** called by 0sql, when there is a reason to call
  out; until then an agent pushes what a tool returned.
- **Incremental pulls** (append past a cursor, or upsert by a key); a
  pull replaces today.
- **Whether a source keeps the original file** beside the parsed rows,
  to read it again with other options.
- **A pipeline with more than one output.**
- **Checks beyond `unique` and `not_null`:** a row count within a share
  of the last run's.
- **Linking a dataset's columns to the model's fields;** suggestions
  drawn from a run's own data ("37 blanks in owner, fill down?"); row
  diffs between output versions.
- **Sending a small key list to the warehouse,** off unless an org turns
  it on.
- **Other formats:** Parquet, Excel, JSON lines.
- **Quotas.**
- **More than one node:** a concern for `docs/clustering.md`.

## Phases

- [x] **1. The core, fed by an upload.** `.datasets` under each project
  and the test that pins it; the crate, the CSV contract, roles and
  types, snapshots, the caps, lineage records, hard delete, the engine
  with external access off.
- [x] **2. Steps, formulas and the grid.** The formula language and its
  compiler; the steps; the column graph; pages and previews.
- [x] **3. Edits.** Remaps, overrides, orphans and flags.
- [x] **4. A semantic query as a source.**
- [x] **5. Combining datasets.** Joins and appends.
- [x] **6. Blends.** Attachment in the request, the isolation golden
  test, `filter`, `enrich` as a look-up and as budget against actual,
  every model key asked for, cross joins, the cache. MCP went to "Later".
- [x] **7. The object-store backup** and restore.
- [ ] **8. Feeds and sharing,** designed then. Sharing stays the
  application's; pushed rows came with phase 9.
- [x] **9. Sources, datasets and pipelines.** Resume dropped; sources
  and snapshots, `push`; a graph sent and run, its checks, the dry run
  and the preview; zero-copy outputs; edits inline by key; the output
  contract; the per-node record, summaries and profiles; the API; the
  recipe, the edit log, held datasets and the cascade removed.

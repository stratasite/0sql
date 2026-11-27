# Data prep demo app

A reference for applications that put 0sql's datasets in front of
their users. A user signs in, imports a CSV, shapes it in a pipeline,
edits cells, joins another dataset, uploads a new version of the file,
and blends it with the model.
TypeScript and Fastify on the backend; plain HTML and JavaScript in the
browser, so the backend, which is the part to copy, stays the thing to
read.

What datasets are, and every rule behind these calls, is in
[`docs/ingestion.md`](../../docs/ingestion.md).

## The shape of it

```
browser ──(this app's session)──► this app's backend ──(only what it allows)──► zsqld
```

The sign-in is a stub: anyone may sign in as any name. Replace
`POST /api/session` with your own; nothing behind it changes.

**zsqld keeps no user.** A dataset is its project's, addressed by its
id, and zsqld answers whoever reaches it, as it does for deploys and
runs. Access control is this app's, and it is all in one place:

- `src/registry.ts` keeps who imported each dataset, and who it is
  shared with. The owner may read, change, share and delete it; a user
  it is shared with may read it and use it in a blend.
- `allowed()` in `src/app.ts` checks that record on every route before
  anything reaches zsqld. A dataset a user may not see answers 404; a
  change by someone it is only shared with answers 403. Tests hold
  that neither reaches zsqld.
- A pipeline reads its inputs: sources and other datasets, each a node
  of its graph. A preview would show them and a run would copy them, so
  the app checks that the user may read every one a graph names, as it
  checks a blend's, before zsqld is asked. zsqld says the same back:
  every run, dry run and preview answers `reads`, each source and
  dataset by kind and id. A user may build on a dataset shared with
  them, and on their own sources; sharing a dataset shares its rows,
  not the sources it was made from.
- zsqld lists every dataset of the project; the app shows each user the
  ones its record lets them.
- zsqld must not be reachable from a browser, or from anyone but this
  backend: whoever reaches it can read every dataset of the org.

Sharing is this app's to model however it likes. The demo shares a
dataset with a user by name. A real app might let a dataset be read by
anyone who may see a chart built on it: the chart's sharing rule
decides, and zsqld is not asked.

**zsqld keeps no pipeline either.** It runs the graph it is sent. The
app keeps each dataset's graph and every version of it
(`src/pipelines.ts`), places a user's grid edits in it, and decides when
it runs: when a user saves a change, edits a cell, or uploads a new
file. A new file runs nothing in zsqld; the app runs the pipeline after
it.

**One thing to say before sharing:** a dataset made from a query source
holds what the context of the run that pulled it could see. Whoever it
is shared with sees those rows, whatever their own context.

## Files

| file | what it holds |
|---|---|
| `src/zsql.ts` | **the client to copy**: every data prep call, typed, with `ZsqlError` and a retry helper; no dependency beyond Node's `fetch` |
| `src/registry.ts` | who owns and may read which dataset and source: this app's records, in a JSON file for the demo |
| `src/pipelines.ts` | each dataset's pipeline: the graph and every version of it, in a JSON file for the demo |
| `src/app.ts` | the backend: a demo sign-in, the access check, and one route per call, forwarding to zsqld |
| `src/config.ts` | what the app reads from the environment |
| `public/` | the browser side: list, import, grid with cell edits, sharing, the pipeline with what each node did, lineage, blend |
| `test/app.test.ts` | the app against a stand-in zsqld: what it forwards, and what its access check keeps from zsqld |

## Run it

zsqld needs the project deployed: datasets belong to a project, which
must be deployed on some branch. They belong to the project, not to the
branch: a deploy or a branch removal never touches them.

```sh
# 1. zsqld, with the grocery-store fixture deployed (from the repository root)
cd fixtures/projects/grocery-store && zsql serve

# 2. this app
cd examples/data-prep-app
npm install
npm start          # http://127.0.0.1:3700
```

| variable | default | what |
|---|---|---|
| `ZSQLD_URL` | `http://127.0.0.1:3699` | where zsqld is |
| `ZSQL_ORG` | `default` | the org the datasets are in |
| `ZSQL_PROJECT` | `grocery-store` | the project they belong to |
| `ZSQL_BRANCH` | `main` | the branch a blend plans the model's part on; blends only. `zsql serve` deploys the branch checked out |
| `SESSION_SECRET` | random, per process | signs the session cookie; unset, sign-ins end when the app stops |
| `DATA_DIR` | `data` | where the app keeps its own records (`registry.json`, `pipelines.json`) |
| `MAX_UPLOAD_BYTES` | 64 MB | the largest file accepted; zsqld has its own cap, `ZSQL_DATASET_MAX_BYTES` |
| `PORT` | `3700` | this app's port |

`npm test` runs the tests; `npm run typecheck` checks the types.

## The calls, as the app makes them

**Import.** The browser posts a multipart form to the app. The app
streams the file to zsqld as the raw body, never holding it whole, as a
source and a dataset over it as it is, which copies nothing:

```ts
const file = await req.file();
const made = await zsql.createUpload(file.file, { name, fileName: file.filename, dataset: true });
registry.add(made.source.id, user.name, { kind: "source" });
registry.add(made.dataset!.id, user.name, { kind: "dataset", source: made.source.id });
pipelines.save(made.dataset!.id, asItIs(made.source.id), user.name);
```

```http
POST /orgs/{org}/projects/{project}/sources?kind=upload&dataset=true&name=Q1%20budget&file_name=budget.csv
content-type: text/csv

<the file's bytes>
```

The answer is the source, its first snapshot (how the file was parsed)
and the dataset, whose ids the app records against the user.

**The pipeline** is a graph the app keeps: nodes from the inputs to one
output, each with an id of the app's choosing.

```json
{ "nodes": [
  { "id": "source", "kind": "source", "source": "src_…" },
  { "id": "steps", "kind": "steps", "input": "source", "steps": [{ "op": "trim_val", "columns": ["region"] }] },
  { "id": "output", "kind": "output", "input": "steps" }
] }
```

Saving a change runs it: `POST /runs` with the graph, the dataset and a
run id the app makes. The same id sent again is answered with what the
first did, so a retry never runs it twice. The answer is the run's
record: what each input read, and for each node its rows, columns
added and removed, each step's values changed ("trimmed 412 values"),
a join's matches, each edit's state, the output's checks, a profile of
each column, and a one-line summary written from those numbers. The
Pipeline tab shows it on every node. A graph zsqld refuses before it
runs comes back 422 with `problems`, each of its node, with the column
it reads that is not there and the ones it may have meant; the app
keeps no version of it.

**A dry run** (`check`) says what a run would read and do, and every
problem, running nothing. **A preview** runs the graph up to a node over
a sample, keeping nothing: what to show while a step is being made.

**The grid** reads pages with `rows`. Sorting and filtering there are
for viewing and change nothing. An edit made there goes in an `edits`
node the app places before the output, carried in the graph, naming its
row by value:

- every row with the value: `{kind: "remap", column, from, to}`;
- this row: `{kind: "set", column, key: {…}, to, was}`, the row named by
  its dimensions, which are the node's key. With `was`, a later file
  that changes the value it replaced is flagged.

Each edit's state after the run (`applied`, `orphaned`, `flagged`) is in
the run's record. Undo takes the edit out of the graph and runs it.

**A new version of the file** is a new snapshot of its source, which
runs nothing; the app then runs the dataset's pipeline over it, and the
record says what changed: a column gone that a step reads refuses the
run, naming it, and the last output stays.

**Blends** run the model's part in the warehouse, so the branch's
datasource must be reachable, with its credential deployed. The app
builds the request to zsqld from what a user may choose (the spec, the
datasets, the joins and a page) and nothing else from the browser: a
browser's `context` would pick its own security context, and its
`connection` would point zsqld at a host of its choosing. Set `context`
from your session, as `contextOf()` in `src/app.ts` is for runs. The
request states every join; nothing is joined for it. Every model key a
join names must be projected too: the dataset is at that grain, so the
answer is. A join with no keys (`"on": []`) is a cross join, each row of
the dataset beside every row of the model, for a target or a rate. The
Blend tab writes a request that runs as it is, from `GET /api/fields`:
the dataset's dimensions the model has by the same name are the keys, or
a cross join when there are none.

## Errors and retries

zsqld refuses in one shape, `{"error": {"class", "message"}}`, with
`problems` beside it for a graph refused, and its messages are written
to be shown to a user. The app passes them through with their status.
`withRetry` tries once more, after the `retry-after` zsqld gives, on
`503 Busy`: nothing was done. A run is safe to send again in any case,
under its id. A streamed upload is not retried: its body cannot be sent
twice.

A run answers when it is over, or 202 past its `wait`, and goes on; read
it again by its id. Give the client a timeout above that wait;
`src/zsql.ts` uses 100 seconds.

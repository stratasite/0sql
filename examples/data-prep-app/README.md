# Data prep demo app

A reference for applications that put 0sql's datasets in front of
their users. A user signs in, imports a CSV, shapes it in a grid, edits
cells, uploads a new version of the file, and blends it with the model.
TypeScript and Fastify on the backend; plain HTML and JavaScript in the
browser, so the backend, which is the part to copy, stays the thing to
read.

What datasets are, and every rule behind these calls, is in
[`docs/ingestion.md`](../../docs/ingestion.md).

## The shape of it

```
browser ──(this app's session)──► this app's backend ──(only what it allows)──► zsqld
```

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
- zsqld lists every dataset of the project; the app shows each user the
  ones its record lets them.
- zsqld must not be reachable from a browser, or from anyone but this
  backend: whoever reaches it can read every dataset of the org.

Sharing is this app's to model however it likes. The demo shares a
dataset with a user by name. A real app might let a dataset be read by
anyone who may see a chart built on it: the chart's sharing rule
decides, and zsqld is not asked.

**One thing to say before sharing:** a dataset made from a semantic
query holds what its creator's security context could see. Whoever it
is shared with sees those rows, whatever their own context.

## Files

| file | what it holds |
|---|---|
| `src/zsql.ts` | **the client to copy**: every data prep call, typed, with `ZsqlError` and a retry helper; no dependency beyond Node's `fetch` |
| `src/registry.ts` | who owns and may read which dataset: this app's records, in a JSON file for the demo |
| `src/app.ts` | the backend: a demo sign-in, the access check, and one route per call, forwarding to zsqld |
| `src/config.ts` | what the app reads from the environment |
| `public/` | the browser side: list, import, grid with cell edits, sharing, recipe, edit log, lineage, blend |
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
| `SESSION_SECRET` | a demo value | signs the session cookie |
| `DATA_DIR` | `data` | where the app keeps its own records (`registry.json`) |
| `MAX_UPLOAD_BYTES` | 64 MB | the largest file accepted; zsqld has its own cap, `ZSQL_DATASET_MAX_BYTES` |
| `PORT` | `3700` | this app's port |

`npm test` runs the tests; `npm run typecheck` checks the types.

## The calls, as the app makes them

**Import.** The browser posts a multipart form to the app. The app
streams the file to zsqld as the raw body, never holding it whole:

```ts
const file = await req.file();
const dataset = await zsql.create(file.file, { name, fileName: file.filename });
registry.add(dataset.id, user.name);
```

```http
POST /orgs/{org}/projects/{project}/datasets?name=Q1%20budget&file_name=budget.csv
content-type: text/csv

<the file's bytes>
```

The answer is the dataset: its `id`, which the app records against the
user who imported it, its columns with their types and roles, and
`notes`.

**A new version of the file** (`replace`) runs the same recipe and the
same edits over it. Show the user what comes back:

- `notes`: "column 'budget' widened from integer to decimal(2)",
  "column 'owner' is new", "1 edit(s) flagged".
- `held`: columns the recipe reads are gone, so the previous version
  stays live. Show `held.unresolved`, and offer to fix the recipe or to
  `confirm`, which drops the steps that held it.

**The grid** reads pages with `rows`. Sorting and filtering there are
for viewing and change nothing. Clicking a cell opens an edit, sent as
the grid showed the row, with the user's choice of which rows it is for
(`match`, the columns whose values must be this row's):

- every row with the value: `{op: "cell", column, row, to, match: []}`,
  a remap of the column; a dimension edit's default;
- the rows that agree on some other columns:
  `match: ["store"]`, so `GROCERY` → `Grocery North` where `store` is
  `north`, for every such row, now and in every later file;
- this row only: `match` naming its dimensions; a measure edit's default.

An edit that names no row, or several where it was meant for one, is
refused with words to show. Each edit's state afterwards (`applied`,
`orphaned`, `ambiguous`, `flagged`) is in the edit log.

**The recipe and the edits are one list** in the Recipe tab: the steps
in order, and each batch of edits where it applies, which is after the
steps the recipe had when it was made. zsqld keeps them apart, steps as
recipe versions and edits as a log, because they behave differently: a
step is a rule over every row; an edit names its rows, and may find
them gone or changed in the next file. When steps are added or removed
before an edit, zsqld keeps the edit after the steps it was made on.

**Recipe changes** send the whole new recipe with `against`, the
version the user was looking at; a 409 means someone changed it since:
read it again. `preview` runs steps over a sample without keeping
anything, which is what to show while a step is being made.

**Blends** run the model's part in the warehouse, so the branch's
datasource must be reachable, with its credential deployed or sent in
`connection`. The request states every join; nothing is joined for it.
Every model key a join names must be projected too: the dataset is at
that grain, so the answer is. A join with no keys (`"on": []`) is a
cross join, each row of the dataset beside every row of the model, for
a target or a rate. The Blend tab writes a request that runs as it is,
from `GET /api/fields`: the dataset's dimensions the model has by the
same name are the keys, or a cross join when there are none.

## Errors and retries

zsqld refuses in one shape, `{"error": {"class", "message"}}`, and its
messages are written to be shown to a user. The app passes them through
with their status. `withRetry` tries once more, after the `retry-after`
zsqld gives, on `503 Busy` and on `409` for a dataset busy with another
job. A streamed upload is not retried: its body cannot be sent twice.

Uploads and recipe changes finish within the request, so give the
client a timeout above zsqld's 90 seconds for data prep; `src/zsql.ts`
uses 100.

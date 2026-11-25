# Clustering: what more than one node still needs

A list of concerns, started 2026-11-24. Nothing here is designed or
scheduled: cluster work is put off, and this is where what it will have
to answer is collected until then. Add to it as things are found.

`docs/execution.md` is what was built. Two things are shared between
nodes today: the records of runs, in the Postgres table `ZSQL_DB`
names, and the results, in the store `ZSQL_OBJECT_STORE` names.
Everything else below is one node's own.

Each entry says what happens today and how it is known: **read** means
read in the code, **run** means a test shows it. No entry here has been
run on two real machines.

## Deploys

- **A deploy reaches one node.** It is written to the data directory of
  the node that received it and loaded into that node's memory. No
  other node hears of it: a request for the branch that lands elsewhere
  is a 404, or an older model. *Read.*
- **A removal reaches one node,** for the same reason. *Read.*
- **One data directory cannot be shared.** A server takes an exclusive
  lock on `<data-dir>/lock` and a second one given the same directory
  refuses to start, so mounting one directory on every node is not a
  way round the entry above as things stand. And a node that did share
  one would still not reload a model another node swapped in. *Read.*
- **Orgs are directories.** Renaming `default` to give its projects to
  an org is done with the server stopped; on a cluster that is every
  node stopped, and every node's directory renamed. *Read.*

What an answer has to settle: where a deployed model lives so that
every node has it, how a node learns of a deploy it did not receive,
and what a request sees while the nodes disagree.

## Credentials

A deploy carries the secrets of each datasource, and the server keeps
them beside the compiled model as that datasource's credential on that
branch ("The credential of a datasource" in `docs/operating.md`).

- **A credential goes where its deploy goes,** so it is on one node,
  exactly as the model is. Whatever shares the model has to share the
  credential.
- **Shared anywhere but a node's own disk, it needs encrypting.** In a
  bucket or a database table a secret is readable by whoever can read
  that bucket or table. That means a key every node is given, where it
  comes from, and how it is rotated.
- **Rotation is a redeploy,** to every node, of every branch that holds
  the credential.
- **Nodes that disagree run as different identities.** Between a
  rotation reaching one node and reaching the next, the same request
  connects with the old credential or the new one by which node it
  lands on, and both feed one cache.

## Runs

- **A cancel only stops a run on the node asked.** `DELETE
  /results/{id}` on another node finds the record, finds no thread of
  its own holding it, and answers with the record unchanged: the
  statement runs on. *Read.*
- **A wait on another node does not hear the end.** A request that
  joins a run in flight on another node, or polls one with `wait`, is
  woken only by its own node's runs. It waits its whole time and then
  reads the record, even when the run ended at once. *Read.*
- **Seats and the queue are each node's.** `ZSQL_CONCURRENT_RUNS` and
  `ZSQL_QUEUE_MAX` count one node's runs. What the warehouse sees is
  the sum, and nothing caps the sum.
- **So are the admission caps** on planning, exploring and deploys.
- **A node is its hostname.** A node that starts ends every unfinished
  run recorded under its own hostname. Two nodes with one hostname
  would end each other's; a node that comes back under a new name
  leaves its old runs to be ended by the others once they go unmarked
  for six beats. *Read.*
- **Being alive is a timestamp.** A run nobody has marked for six beats
  (a minute) is ended as failed by any node. A node that stalls for
  longer than that, on a full disk or a database that will not answer,
  has its live runs ended under it, and then overwrites them when they
  finish. The marking is done with the database's clock; the nodes'
  own clocks are used for `expires_at`.

## Connections

- **The pool is each node's.** `ZSQL_POOL_MAX` is per node, so a
  warehouse is held open by up to that many connections times the
  number of nodes, and a credential's connections are not shared.
- **A file is on one machine.** A DuckDB or SQLite datasource names a
  path, which has to exist on every node and be the same file. DuckDB
  keeps a writable connection to itself within one process; nothing
  keeps two nodes from opening one file, one of them to write.

## Results and the cache

- **Without the object store there is no shared cache.** A result is on
  the node that made it. Another node finds the record, cannot find the
  rows, and answers 404 for them.
- **The local copy is each node's,** with its own
  `ZSQL_LOCAL_CACHE_MAX_PERCENT` and its own sweep.
- **Every node cleans the store.** Each sweeps the stale objects the
  registry lists, up to 500 at a time. Two doing it at once delete the
  same things twice, which is harmless and wasteful. Whether one node
  should own it is open.
- **Invalidation is shared only with `ZSQL_DB`.** It marks records, so
  with the records on each node's disk it reaches one node.
- **The object store has not been run against a cloud.** Every test of
  it uses a directory. S3, GCS and Azure are untried.

## The agent endpoint

- **The catalog an MCP client is handed is the node's own,** so two
  nodes with different deploys describe different projects to the same
  agent from one connection to the next.

## Running it

- **Nothing says what a node is.** No endpoint lists the nodes, what
  each has deployed, or whether they agree.
- **A rolling upgrade is untried.** Two builds sharing one records
  table and one store: the table's columns are checked at startup, the
  result files carry a format, and nothing has been run across a
  version boundary.
- **The ADBC drivers** have to be installed on every node, at the same
  version, or the same datasource returns typed columns from one node
  and text from the next.

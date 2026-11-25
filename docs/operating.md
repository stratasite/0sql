# Operating a server

For whoever runs `zsql serve` for other people: where it belongs, what
it keeps on disk, what it is told, what to back up, and what changes
shape between releases. [Running the server](https://0sql.io/docs/getting-started/server/)
on the site is the guide to setting one up; this is what the guide
leaves to the operator.

## Where it belongs

The server has no accounts, no users, no keys and no token. It answers whoever
can reach it, the way a database on a private network does, and it is
meant to be used like one: behind your own service, which authenticates
your users, decides which projects each may ask about, sends the
security context each plan runs under, and paces its callers.

| | who does it |
|---|---|
| who may ask, and for which project | the service in front |
| the security context of a plan | the caller sends it: `context` in the body, the `Zsql-Context` header on `/mcp` |
| rate limits | the proxy or the service in front |
| TLS | the proxy |
| who may deploy, validate, test and remove | the service in front, or the network: the server asks nobody |

Do not expose the port to callers you do not trust. Anyone who reaches
it reads every deployed model's catalog, plans SQL under any context
they care to claim, **runs that SQL and reads the rows**, runs a
statement of their own with `sql-exec`, and deploys and removes. "Running the server's statements", below, says
what that is worth.

## No credential of its own

The server checks nothing a request carries. A deploy, a validate, a
test and a removal are as open as a read, on every address it listens
on, and an `Authorization` header is not read. Releases before this
one took a deploy token (`ZSQL_DEPLOY_TOKEN`, sent by the CLI as
`ZSQL_API_KEY` or `zsql auth --api-key`). That is gone: the variable
is ignored, `zsql auth` no longer exists, and an `api_key` left in a
`.zsql` is not read.

So who may deploy is decided where every other question of who is
decided: by the service in front, or by who can reach the address.
There are two ways to run it. Beside the service or proxy that
fronts it, on the same machine, it binds the loopback, which is the
default, and nothing off the machine reaches it. Reached from other
machines, as a node of a cluster or behind a proxy elsewhere, it binds
a private address, and `zsql serve` refuses to do that unless `--open`
or `ZSQL_OPEN=1` says it is meant: nothing else stands between that
address and a deploy.

**Coming from a server that had a token:** it now takes a deploy from
anyone who reaches it. Before upgrading, make sure the write routes
are not forwarded to callers who should not deploy, or that the port
is not reachable by them at all.

## Deploying to it

Deploy to the server directly, the way migrations are run against a
database: on a path of its own, not through whatever the application's
queries pass through. `zsql deploy` goes wherever `server` in
`project.yml` or `ZSQL_SERVER` points, and sends nothing to say who it
is.

| where you deploy from | how it reaches the server |
|---|---|
| the same private network or VPN | the server's private address |
| anywhere else | a tailnet, or `ssh -L 3699:127.0.0.1:3699 <host>` and `ZSQL_SERVER=http://127.0.0.1:3699` |
| hosted CI | a tailnet in the workflow, or the proxy, below |

With deploys on their own path, the proxy in front carries reads only
and should refuse the four write routes outright: `POST
.../deploy`, `.../validate`, `.../test` and `DELETE
/projects/{uid}/branches/{branch}`. A proxy that forwards a fixed list
of read routes does this without trying.

A proxy may carry deploys too. Its own check is then the only thing
between a caller and a deploy: it forwards the write routes only for a
caller it has decided may deploy, and to no one else.

What to keep in mind whichever path is used:

- **A deploy is worth what the warehouse credentials are worth.** A
  model carries SQL expressions that are run with a datasource's
  credentials, and the row policies. Whoever can deploy can drop the
  policies or choose that SQL.
- **Reach is every project.** Nothing scopes a caller: whoever
  reaches the write routes may overwrite or remove any branch of any
  project, and the log does not say who did.
- **The direct path is plain HTTP.** A tailnet or an SSH tunnel
  encrypts it. A flat VPN or VPC may not.
- **Bind the private address, not every address.** Reaching the server
  over a VPN means a bind beyond the loopback, and on a host that also
  has a public interface `0.0.0.0` is the internet. Behind an SSH
  tunnel the loopback bind is enough.
- **Reach is the warehouse.** Any host that can reach the port lists
  every model, plans under any context it claims, and has the plan
  run: it gets rows. A context is whatever the caller says it is, so
  a row policy protects data through the application that sends the
  context, not against the network. Before the server ran anything,
  reach was SQL text and the shape of the warehouse; that is no longer
  so, and a port opened on that understanding should be closed.
- **A probe connects where the deploy and the request say.** `POST
  .../datasources/{ds}/tables` and `.../tables/{table}` read a
  warehouse's own catalog with the connection the datasource was
  deployed with and, over it, the one in the request body: host, user
  and secret, in the keys `datasources.yml` and `.zsql` take. The
  server keeps nothing a request sends, on disk or in a log, and holds
  no connection open afterwards. But it does open the connection, from
  its own network position, to whatever host the body names, and a
  `file`, `ca_file` or `key_file` there is a path on the server. The
  server asks nobody who they are, so the service in front decides
  who may send them and with what: forward them only for a caller it
  would trust with that reach, and build the connection itself rather
  than pass a client's through.
- **A deployed secret is on disk.** A deploy carries the secrets of
  each datasource, and the server keeps them as that datasource's
  credential: in `secrets.json` beside the compiled model, in the
  clear, readable by the server's user alone and in every backup of
  the data directory. They are not in `snapshot.json.gz`. No read
  serves one, and anyone who reaches the port can then run and probe
  that warehouse with it. See "The credential of a datasource".

## Probing a datasource

What `zsql catalog` reads from a project directory, for a caller that
has only the server. A deploy keeps a datasource's entry whole: name,
adapter, tier, `query_timeout`, `extra_query_params`, and every other
key as written. `GET .../datasources` lists each with those under
`connection`, leaving out any secret and naming it under `secrets`.

```
GET  /projects/{uid}/branches/{branch}/datasources                      ?q=
POST /projects/{uid}/branches/{branch}/datasources/{ds}/tables          ?q=&schema=&catalog=&limit=&offset=
POST /projects/{uid}/branches/{branch}/datasources/{ds}/tables/{table}  ?schema=&catalog=
```

Both POSTs take the same body, and it is optional. The connection is
what was deployed, and each key the body sends replaces the deployed
one (`null` unsets it), so a datasource deployed with everything is
probed with no body, one deployed with all but its password is sent
`{"connection": {"password": "…"}}`, and one deployed with only its
adapter is sent the lot. The adapter is the deployed one and need not
be sent; a key in the body that is not a connection setting is refused
by name.

```json
{"connection": {"host": "db.internal", "port": 5432, "database": "analytics",
                "username": "reader", "password": "…", "sslmode": "verify-full"}}
```

The listing is always a page: 100 tables unless `limit` asks for fewer,
by schema and name, each with its kind, the warehouse's row estimate
and its comment. `next_offset` is the `offset` of the next page and
null on the last. `q` keeps the tables whose name holds it, an
underscore meaning an underscore. `schema` narrows to one schema;
without it every schema but the engine's own is listed.

`catalog` is the level above a schema, which engines spell differently
and some lack:

| engine | `catalog` is | `schema` is |
|---|---|---|
| Postgres, Redshift, SQL Server, Snowflake, Trino | the database | the schema |
| Databricks, Athena | the catalog | the schema (Athena: the database) |
| BigQuery | the project | the dataset |
| MySQL, ClickHouse | refused: there is none | the database |
| DuckDB | refused | the schema |
| SQLite | refused | refused: a file has neither |

One table is named in the path as `table`, `schema.table` or
`catalog.schema.table`; with `?schema=` the path is the whole name. The
answer is its columns in order with the warehouse's own types, its
primary and foreign keys, comment and row estimate. A name found in
several schemas is refused with the schemas (`Ambiguous`), not answered
with the first; one found nowhere is `NotFound`. A connection the
request got wrong is `400 DatasourceError`; a warehouse that would not
connect or answer is `502 DatasourceError` with what it said.

## Running the server's statements

The server runs what it plans. `POST .../run` plans a request and runs
the statement on its datasource; `POST .../sql-exec` runs one
statement the caller wrote; `GET /results/{id}` reads a result again
and `DELETE /results/{id}` cancels a run. `docs/execution.md` is the
design.

**Whose credential.** The one the request sends, in `connection`, and
otherwise the one the datasource was deployed with. The service in
front decides which: it can send each user's own, or send none and let
every user run as the deployed account.

- **A credential the server holds is worth whatever the database
  grants it, to everyone who reaches the port.** With one set, a
  caller needs nothing but reach to read through `run`, and to do
  anything that account may do through `sql-exec`.
- **Nothing here stops a write.** No statement is checked for being a
  read. What an account may do is the database's to enforce: give the
  server an account that may only read, unless writing is the point.
- **`sql-exec` is outside the model.** It runs the caller's SQL as
  written, so no row policy applies to it. If only planned queries
  should run, do not forward that route.
- **The MCP tools `run` and `result` are off** unless `ZSQL_MCP_TOOLS`
  names them. A model has no credential to send, so it runs as the
  server's account.

**The cache.** A planned query's result answers the same question
again until the datasource's `cache_ttl` passes. It is found by the
project and the statement, nothing else: not the user, the credential,
the datasource or the branch. Two consequences to plan for:

- 0sql's row policies are in the statement, so two users under
  different policies never share a result. Policies the *warehouse*
  applies per user are not seen: two users with their own credentials
  and the same statement get the first one's rows. Use `cache_ttl: 0`
  on such a datasource, which keeps nothing.
- A branch whose datasource points at another database shares results
  with the others of its project. `cache_ttl: 0` there as well.

`fresh: true` on a request runs the statement whatever is stored.
`POST /cache/invalidate` with a datasource and the tables that changed
stops every result that read them from answering.

**Limits are the caller's.** A query with no limit returns every row;
`ZSQL_DEFAULT_SQL_LIMIT` gives one to requests that have none. Rows go
to disk as they arrive, so a large result costs disk and time, not
memory.

**More than one node** needs `ZSQL_DB` for the records and
`ZSQL_OBJECT_STORE` for the rows. Without the store a result is only
on the node that ran it, and another node runs the statement again.

## The credential of a datasource

A datasource's credential arrives with a deploy. There is no setting
for one on the server: `ZSQL_DS_<UID>_<FIELD>` in the server's
environment is not read. (The one exception is the cloud identity of
the server's own process, below.)

- **A deploy carries it.** `zsql deploy` sends, in each datasource's
  entry, the secrets written there and the ones the deploying machine
  holds for it in `.zsql`, its environment or the home config: what a
  query run on that machine connects with. `.zsql` itself is not sent.
  So whoever deploys sets who the server runs as. A developer's own
  account deployed from a laptop is that, for every caller, until a
  deploy carries another or the service in front sends one with each
  request.
- **It is one branch's.** The credential belongs to that datasource on
  that branch of that project in that org.
- **A request's own replaces it whole.** A request that sends any
  secret in `connection` connects with exactly the secrets it sent;
  nothing deployed is blended in. Settings that are not secrets, such
  as `role`, are still laid over the deployed entry one by one.
- **It goes only where it was deployed for.** A request may lay
  `schema`, `dataset`, `warehouse`, `role`, `workgroup`,
  `output_location`, `cache_ttl`, `query_wait_time` or `query_timeout`
  over the entry and still connect with the deployed credential. A
  request that sends any other setting, a `host`, a `username`, a
  `file`, an `sslmode`, connects with the secrets it sent and no
  deployed one: otherwise anyone who reaches the port could have the
  deployed secret presented to a server of their choosing.
- **An identifier alone is not a credential.** `oauth_client_id` and
  `access_key_id` are often written in `datasources.yml` with the
  secret beside them in `.zsql`. A deploy that carries only those keeps
  the secret the branch had, under the identifier it carried.
- **A validation carries none.** `zsql deploy --dry-run` and `zsql
  check -r` send the datasources with every secret taken out.
- **What the server's own account can reach is not a deployed
  credential, and is everyone's.** With no secret at all, Athena falls
  back to the AWS identity of the server's process (its environment,
  profile or instance role) and BigQuery to its application-default
  credentials. Any org can deploy a datasource that uses them. Run the
  server as an identity that can reach nothing you would not give
  every caller.
- **A deploy that carries none keeps it,** so a pipeline can deploy a
  model without holding the secret. It is kept only while the
  datasource points where it did: a changed adapter, host, database,
  account, username or any other setting but `schema`, `dataset`,
  `warehouse`, `role`, `workgroup`, `output_location`, `sslmode`,
  `ca_file`, `cache_ttl` and `query_wait_time` drops it, so a credential never follows a
  datasource somewhere new.
- **A branch deployed for the first time has none.** It borrows from
  no other branch, the production branch included.
- **To take one away,** deploy with `zsql deploy --clear-secrets`
  (`?clear_secrets=true`), which leaves a datasource the deploy carries
  no secret for with none; or remove the branch. To rotate one, deploy
  again with the new secret.
- **The answer to a deploy says what became of each:** `credentials`
  maps every datasource to `set`, `kept`, `dropped` or `none`. `GET
  .../datasources` names the secrets held and shows none.
- **Send it over TLS.** A deploy now carries a secret through whatever
  is in front of the server.
- **It is on one node.** A deploy reaches the node it was sent to; see
  `docs/clustering.md`.

## Orgs

A project belongs to an org, and a server may keep the projects of
several. An org is a name and a directory, and nothing else: the
server has no list of orgs, no settings for one and no idea who is in
it. Two orgs may each have a project of the same uid.

- **The org is in the path.** Every route that is about a project, a
  result or the cache, and the MCP endpoint, is served under
  `/orgs/{org}`: `/orgs/acme/projects/shop/branches/main/sql`,
  `/orgs/acme/results/{id}`, `/orgs/acme/cache/invalidate`,
  `/orgs/acme/mcp`.
- **With no org in the path, the org is `default`.**
  `/projects/shop/branches/main/sql` and
  `/orgs/default/projects/shop/branches/main/sql` are the same request.
- **A name** is lower-case letters, digits, `-` and `_`, as a project's
  uid is. An org exists once something is deployed to it.
- **`zsql` takes the org from `org:` in `project.yml`**, or from
  `--org`, which wins. With neither, the default org.
- **Nothing crosses.** A listing, the catalog an MCP client is handed
  and the branch a tool falls back to are one org's. A result is its
  org's alone: asked for or cancelled under another org it answers 404,
  in the words an unknown id gets. A stored result answers the same
  question in its own org only, an invalidation reaches one org's
  results, and no warehouse connection is reused between two.
- **A credential is one org's.** It is deployed with a datasource, to
  one branch of one project of one org, and is that branch's alone.
- **Who may ask for which org is decided in front**, like everything
  else. A proxy that keeps orgs apart allows each caller the one prefix
  `/orgs/<theirs>/` and nothing outside it; the unprefixed routes are
  the default org's, and `GET /orgs`, which lists every org with
  something deployed, is for whoever runs the server.
- **To give the default org's projects to a named one**, stop the
  server, rename `default` in the data directory to the org's name,
  and start it: the directory's name is the org. An empty `default` is
  made again at startup. Results stored before the rename stay the
  default org's and are run again when next asked for.

## What is on disk

Everything the server keeps lives in one data directory: `--data-dir`,
`ZSQL_DATA_DIR`, else `$ZSQL_HOME/data`.

| | |
|---|---|
| `<org>/<project>/<branch>/` | a deployed branch: the compiled model (`snapshot.json.gz`), its tests and sampled values. `default/` is the org of a deploy that names none, and is made at startup |
| `<org>/<project>/<branch>/secrets.json` | the secrets the branch's datasources were deployed with, in the clear, readable by the server's user alone; absent when there are none |
| `lock` | held by the running server; its pid, address and public URL, which `zsql server status` reads |
| `.cache/results/` | the rows of each run the server made, an Arrow file each |
| `.cache/records/` | the record of each run, when the records are not in a database |

A deploy is staged beside the branch and swapped in by rename, so a
reader of the directory sees a branch as it was or as it is, never half
written. A restart reloads every branch from disk in well under a
second.

## What it is told

`ZSQL_*` variables are the base and flags win over them; the library
itself reads no environment, so a flag and a variable can only mean the
same thing. The ones an operator sets:

| variable | what |
|---|---|
| `ZSQL_BIND` | the address; `127.0.0.1:3699` by default, and a proxy in front is the usual arrangement |
| `ZSQL_OPEN` | `1` to allow a bind beyond the loopback, the same as `--open`: for a node other machines reach. It checks no caller; it says the exposure is meant |
| `ZSQL_DATA_DIR` | the data directory |
| `ZSQL_LOG`, `ZSQL_LOG_FORMAT` | what is logged and how; the log, below |
| `ZSQL_PUBLIC_URL` | where the server is reached from outside, for `zsql server status` to report |
| `ZSQL_ADMIT_PLAN`, `ZSQL_ADMIT_EXPLORE`, `ZSQL_ADMIT_HEAVY`, `ZSQL_ADMIT_PROBE` | how much of each kind of work runs at once; `docs/admission.md` |
| `ZSQL_DB`, `ZSQL_DB_USER`, `ZSQL_DB_PASSWORD` | a Postgres to keep the records of runs in, as `host:port/database` with `?sslmode=` when it says; unset, they are kept on this node's disk. The table is `zsql_results`, made from `ops/zsql_results.sql` by whoever runs the database: the server checks for it at startup and never creates it |
| `ZSQL_OBJECT_STORE` | where results are kept for every node to read: `s3://bucket/prefix`, `gs://`, `az://`, or `file:///dir`. A result goes under `cache/` there. Unset, a result is only on the disk of the node that made it |
| `ZSQL_OBJECT_STORE_<OPTION>` | the store's options, by the store's own names: `ZSQL_OBJECT_STORE_AWS_ACCESS_KEY_ID`, `ZSQL_OBJECT_STORE_AWS_SECRET_ACCESS_KEY`, `ZSQL_OBJECT_STORE_AWS_REGION`, `ZSQL_OBJECT_STORE_AWS_ENDPOINT` and so on. An option the store does not have stops the server at startup. `AWS_*` and `GOOGLE_APPLICATION_CREDENTIALS` are never read for the store: the Athena and BigQuery adapters fall back to them, and a storage credential there would become a warehouse login |
| `ZSQL_LOCAL_CACHE_MAX_PERCENT` | the share of the disk this node's results may take; 60. Past it, with an object store, the least recently read local copies are removed; without one, a run fails saying the disk is full |
| `ZSQL_CACHE_SWEEP_SECS` | how often results that no longer answer, expired or invalidated, are deleted from the store and the disk; 300 |
| `ZSQL_POOL_MAX` | warehouse connections kept open for reuse, across every datasource; 200. A connection is reused only by a run with the same datasource, settings and credential |
| `ZSQL_POOL_IDLE_SECS` | how long a connection nothing uses is kept; 60 |
| `ZSQL_CONCURRENT_RUNS` | how many statements run at once; 100 |
| `ZSQL_QUEUE_MAX` | how many runs may wait for a seat before the next is refused as busy; 5,000 |
| `ZSQL_DEFAULT_SQL_LIMIT` | the row limit a run is given when its request has none; unset, which is no limit |
| `ZSQL_MCP_TOOLS`, `ZSQL_MCP_PROMPTS`, `ZSQL_MCP_SPEC_SCHEMA` | the MCP tools exposed, and files to read the instruction texts from in place of the compiled-in ones |

`ops/zsqld.service` is a systemd unit to start from: a dedicated user, a
state directory systemd owns, no capabilities, a read-only filesystem
apart from that directory, a private temp directory and a system call
filter. The `ZSQL_*` variables go in its `EnvironmentFile`, mode 0600.

## The log

Everything 0sql reports goes to stderr through one logger
(`crates/log`), which a systemd unit's journal collects and stamps.

| variable | flag | what |
|---|---|---|
| `ZSQL_LOG` | `--log` | a level (`off`, `error`, `warn`, `info`, `debug`, `trace`), or levels by crate: `warn,zsql_datasource=debug`. A name matches by prefix, so `zsql=debug` is every crate of 0sql and no library's. `zsql serve` logs at `info` unless told otherwise |
| `ZSQL_LOG_FORMAT` | `--log-format` | `plain`, the default, or `json`: an object a line with `ts`, `level`, `target`, `message` and each field under its own name |
| | `-v`, `-vv`, `-vvv` | shorthand: what 0sql did, how, and every library too |

A plain line is the program's name and the message, then any fields as
`key=value`. A warning or an error says so (`zsqld: warning: ...`), and
anything finer than `info` names the crate it came from. A value that
is a credential is never written: it prints as `Secret(…)`.

Every other command of `zsql` logs nothing unless asked with the same
flags and variables. What a command prints for whoever ran it, a
result, a `warning:` line, an error, is its output and not its log,
and does not change with the level.

`zsql server start` finds where the server listens from the line below
or, when the log is quieter than `info` or is JSON, from the lock file.
The background server takes `ZSQL_LOG` and `ZSQL_LOG_FORMAT` from the
environment it is started in.

## The first lines it logs

```
zsqld: 9 deployment(s) loaded from /var/lib/zsqld
zsqld: listening on http://127.0.0.1:3699
```

After a restart, compare the first line with the previous one: the
number of deployments. A branch this build cannot load is
logged by name with its reason, left out, and counted in a line after
the summary saying what is not being served; everything else comes up.
The fix for one is to deploy that branch again from its repository,
since the compiled model is never migrated.

Each deploy and each removal writes a line naming the project and
branch. That line is the only record of who changed what, and it does
not say who: the server asks nobody.

## Backups

Back up the data directory. It holds nothing that cannot be rebuilt:
every deployed branch was compiled from a project in somebody's
repository, and deploying it again restores it. A backup saves doing
that for each one. Restoring is stopping the server, putting the
directory back, fixing ownership and starting it.

## Coming from a release with accounts

Releases before 0.6.10 could run with `ZSQL_ACCOUNTS=on`: users, keys,
sign-up, a web app, rate limits, all in the server. That is gone.

- `ZSQL_ACCOUNTS` is no longer read, and nothing stops a server that
  still sets it: one that authenticated every request comes back up
  answering anyone who reaches it. Remove it from the environment
  file, with `ZSQL_SIGNUP`, `ZSQL_LIMITS`, `ZSQL_LIMIT_*`, `ZSQL_MAIL_*`,
  `ZSQL_TRUSTED_PROXY`, `ZSQL_AUTH_CACHE_TTL`, `ZSQL_MAX_BRANCHES_*`,
  `ZSQL_ADMIT_MANAGE` and `ZSQL_ADMIT_PASSWORD`, which are no longer
  read. `ZSQL_DEPLOY_TOKEN`, which that release introduced, is not
  read either: see "No credential of its own", above.
- Deployments lived under `<account>/<project>/<branch>/` and are now
  looked for at `<project>/<branch>/`. Deploy each again, or move each
  `<account>/<project>` directory up one level; two accounts with a
  project of the same uid cannot both be kept.
- `accounts.db` is not read, and nothing says so. Keep a copy if the
  users and audit log in it matter to you, then delete it.
- User keys and query keys stop working: nothing checks them. An
  application that sent a query key keeps working, and so does anyone
  else. A context that was bound to a query key's grant must now be
  sent by the caller.
- `/app`, `/login`, `/signup`, `/me`, `/account/...` and
  `/projects/{uid}/access` answer 404.

## What changes shape between releases

**The data directory.** A deployed branch is kept at
`<org>/<project>/<branch>/`. A server from before orgs kept it at
`<project>/<branch>/`, and nothing moves it: a newer server leaves
such a directory where it is, does not serve it, and names it at
startup. Stop the server and move each `<project>` directory into
`default/`, or deploy each branch again. The paths with no org in them
answer as they did, for the default org.

**A datasource's credential.** A server before this one read it from
`ZSQL_DS_<UID>_<FIELD>` in its own environment. Those are not read now,
and nothing says so: a run that relied on one is refused for want of a
password until the branch is deployed again from a machine that holds
the secret. A secret that an older deploy left inside
`snapshot.json.gz` is still used, and moves to `secrets.json` the next
time that branch is deployed.

**The compiled model.** `snapshot.json.gz` carries a `version`, and
`zsql_semantic::snapshot::FORMAT` is the newest a build writes and
reads. Nothing migrates a snapshot and nothing needs to: it is compiled
from a project in somebody's repository, so the fix for one a build
cannot read is to deploy that branch again, and startup says which.

## Memory

A deployed model is held in memory for the life of the process and
nothing evicts it: about 5 MB for a model of the TPC-DS sample's size.
Nothing caps how many are deployed either, so whoever can deploy can
fill memory, and a process killed for memory restarts into
the same reload. Size the machine for the branches it serves, or set
`MemoryMax` and watch the journal. Lazy loading with an LRU is the open
design here.

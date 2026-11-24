# Operating a server

For whoever runs `zsql serve` for other people: where it belongs, what
it keeps on disk, what it is told, what to back up, and what changes
shape between releases. [Running the server](https://0sql.io/docs/getting-started/server/)
on the site is the guide to setting one up; this is what the guide
leaves to the operator.

## Where it belongs

The server has no accounts, no users and no keys. It answers whoever
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
| who may deploy | the deploy token, below |

Do not expose the port to callers you do not trust. Anyone who reaches
it reads every deployed model's catalog, plans SQL under any context
they care to claim, and, without a deploy token, deploys and removes.

## The deploy token

`ZSQL_DEPLOY_TOKEN` in the server's environment. With it set, four
routes need `Authorization: Bearer <token>`: deploy, validate, test and
remove. Every read stays open. Without it those four are as open as the
reads, which is right on a loopback and nowhere else, so `zsql serve`
refuses a bind beyond the loopback unless the token is set or `--open`
says the exposure is meant.

The CLI sends the token as it always sent a key: `zsql auth --api-key
<token>` saves it in the project's `.zsql`, and `ZSQL_API_KEY` in the
environment wins over that. A server serving its own project
(`zsql serve` inside one) deploys it with its own token.

The token is one shared secret: a long random string
(`openssl rand -hex 32`), in the environment file at mode 0600, never
on a command line. It is compared in constant time and never logged.
To rotate it, change the variable and restart; there is one token at a
time.

## Deploying to it

Deploy to the server directly, the way migrations are run against a
database: on a path of its own, not through whatever the application's
queries pass through. `zsql deploy` goes wherever `server` in
`project.yml` or `ZSQL_SERVER` points, with the token from `.zsql` or
`ZSQL_API_KEY`.

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

A proxy may carry deploys too, in one of two ways:

- **It passes `Authorization` through** on the write routes, and the
  server checks the token as if the caller had come direct. The proxy
  cannot use `Authorization` for a credential of its own on those
  routes: the CLI sends one such header.
- **It holds the token and adds it itself**, so its users deploy
  through the interface they already have. The proxy's own check is
  then the only thing between a caller and a deploy. It must add the
  token on the write routes alone, and only for a caller it has decided
  may deploy; added to every forwarded request, the token gives a
  deploy to everyone who can read.

What to keep in mind whichever path is used:

- **The token is worth what the warehouse credentials are worth.** A
  model carries SQL expressions the application will run with its own
  credentials, and the row policies. Whoever can deploy can drop the
  policies or choose that SQL.
- **One token is every project.** It is not scoped: its holder may
  overwrite or remove any branch of any project, and the log does not
  say which holder did.
- **The direct path is plain HTTP.** A tailnet or an SSH tunnel
  encrypts it. A flat VPN or VPC may not, and then the token crosses it
  in the clear on every deploy.
- **Bind the private address, not every address.** Reaching the server
  over a VPN means a bind beyond the loopback, and on a host that also
  has a public interface `0.0.0.0` is the internet. Behind an SSH
  tunnel the loopback bind is enough.
- **Reach is read access.** Any host that can reach the port lists
  every model and plans under any context it claims. It gets SQL text
  and the shape of the warehouse, not rows; policies protect data
  through the application that runs the SQL, not against the network.
- **A probe connects where the deploy and the request say.** `POST
  .../datasources/{ds}/tables` and `.../tables/{table}` read a
  warehouse's own catalog with the connection the datasource was
  deployed with and, over it, the one in the request body: host, user
  and secret, in the keys `datasources.yml` and `.zsql` take. The
  server keeps nothing a request sends, on disk or in a log, and holds
  no connection open afterwards. But it does open the connection, from
  its own network position, to whatever host the body names, and a
  `file`, `ca_file` or `key_file` there is a path on the server. These
  routes are reads and take no token, so the service in front decides
  who may send them and with what: forward them only for a caller it
  would trust with that reach, and build the connection itself rather
  than pass a client's through.
- **A deployed secret is on disk.** A deploy keeps every key of a
  datasource's entry. `zsql deploy` strips the secret-shaped ones
  before it sends, so the CLI never puts one on the server; an archive
  built another way that carries a `password` has it kept in
  `snapshot.json.gz`, in the clear, readable by the server's user and
  in every backup of the data directory. No read serves it, and anyone
  who reaches the port can then probe that warehouse with it.

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

## What is on disk

Everything the server keeps lives in one data directory: `--data-dir`,
`ZSQL_DATA_DIR`, else `$ZSQL_HOME/data`.

| | |
|---|---|
| `<project>/<branch>/` | a deployed branch: the compiled model (`snapshot.json.gz`), its tests and sampled values |
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
| `ZSQL_DATA_DIR` | the data directory |
| `ZSQL_DEPLOY_TOKEN` | what a deploy, a validate, a test and a removal must present; a variable only, never a flag |
| `ZSQL_LOG`, `ZSQL_LOG_FORMAT` | what is logged and how; the log, below |
| `ZSQL_PUBLIC_URL` | where the server is reached from outside, for `zsql server status` to report |
| `ZSQL_ADMIT_PLAN`, `ZSQL_ADMIT_EXPLORE`, `ZSQL_ADMIT_HEAVY`, `ZSQL_ADMIT_PROBE` | how much of each kind of work runs at once; `docs/admission.md` |
| `ZSQL_CONCURRENT_RUNS` | how many statements run at once; 100 |
| `ZSQL_QUEUE_MAX` | how many runs may wait for a seat before the next is refused as busy; 5,000 |
| `ZSQL_DEFAULT_SQL_LIMIT` | the row limit a run is given when its request has none; unset, which is no limit |
| `ZSQL_DS_<UID>_<FIELD>` | the credential the server holds for a datasource, used by a run whose request sends none |
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
zsqld: 9 deployment(s) loaded from /var/lib/zsqld; deploys need the deploy token
zsqld: listening on http://127.0.0.1:3699
```

After a restart, compare the first line with the previous one: the
number of deployments, and whether deploys need the token or `are open
to whoever reaches this address`. A branch this build cannot load is
logged by name with its reason, left out, and counted in a line after
the summary saying what is not being served; everything else comes up.
The fix for one is to deploy that branch again from its repository,
since the compiled model is never migrated.

Each deploy and each removal writes a line naming the project and
branch. That line is the only record of who changed what, and it does
not say who: everyone who deploys holds the same token.

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
  read. Set `ZSQL_DEPLOY_TOKEN`.
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

**The compiled model.** `snapshot.json.gz` carries a `version`, and
`zsql_semantic::snapshot::FORMAT` is the newest a build writes and
reads. Nothing migrates a snapshot and nothing needs to: it is compiled
from a project in somebody's repository, so the fix for one a build
cannot read is to deploy that branch again, and startup says which.

## Memory

A deployed model is held in memory for the life of the process and
nothing evicts it: about 5 MB for a model of the TPC-DS sample's size.
Nothing caps how many are deployed either, so whoever holds the deploy
token can fill memory, and a process killed for memory restarts into
the same reload. Size the machine for the branches it serves, or set
`MemoryMax` and watch the journal. Lazy loading with an LRU is the open
design here.

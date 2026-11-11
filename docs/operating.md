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

## What is on disk

Everything the server keeps lives in one data directory: `--data-dir`,
`ZSQL_DATA_DIR`, else `$ZSQL_HOME/data`.

| | |
|---|---|
| `<project>/<branch>/` | a deployed branch: the compiled model (`snapshot.json.gz`), its tests and sampled values |
| `lock` | held by the running server; its pid, address and public URL, which `zsql server status` reads |

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
| `ZSQL_PUBLIC_URL` | where the server is reached from outside, for `zsql server status` to report |
| `ZSQL_ADMIT_PLAN`, `ZSQL_ADMIT_EXPLORE`, `ZSQL_ADMIT_HEAVY` | how much of each kind of work runs at once; `docs/admission.md` |
| `ZSQL_MCP_TOOLS`, `ZSQL_MCP_PROMPTS`, `ZSQL_MCP_SPEC_SCHEMA` | the MCP tools exposed, and files to read the instruction texts from in place of the compiled-in ones |

`ops/zsqld.service` is a systemd unit to start from: a dedicated user, a
state directory systemd owns, no capabilities, a read-only filesystem
apart from that directory, a private temp directory and a system call
filter. The `ZSQL_*` variables go in its `EnvironmentFile`, mode 0600.

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

- `zsql serve` refuses to start while `ZSQL_ACCOUNTS` is on, so a
  server that authenticated every request does not come back up
  answering anyone. Remove it from the environment file, with
  `ZSQL_SIGNUP`, `ZSQL_LIMITS`, `ZSQL_LIMIT_*`, `ZSQL_MAIL_*`,
  `ZSQL_TRUSTED_PROXY`, `ZSQL_AUTH_CACHE_TTL`, `ZSQL_MAX_BRANCHES_*`,
  `ZSQL_ADMIT_MANAGE` and `ZSQL_ADMIT_PASSWORD`, which are no longer
  read. Set `ZSQL_DEPLOY_TOKEN`.
- Deployments lived under `<account>/<project>/<branch>/` and are now
  looked for at `<project>/<branch>/`. Deploy each again, or move each
  `<account>/<project>` directory up one level; two accounts with a
  project of the same uid cannot both be kept.
- `accounts.db` is not read. The server says so at startup while the
  file is there. Keep a copy if the users and audit log in it matter to
  you, then delete it.
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

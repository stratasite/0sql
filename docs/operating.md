# Operating a server

For whoever runs `zsql serve` for other people: what it keeps on disk,
what it is told, what to back up, and what changes shape between
releases. [Running the server](https://0sql.io/docs/getting-started/server/)
on the site is the guide to setting one up; this is what the guide
leaves to the operator.

## What is on disk

Everything the server keeps lives in one data directory: `--data-dir`,
`ZSQL_DATA_DIR`, else `$ZSQL_HOME/data`.

| | |
|---|---|
| `<account>/<project>/<branch>/` | a deployed branch: the compiled model (`snapshot.json.gz`), its tests and sampled values; `<project>/<branch>/` with accounts off |
| `accounts.db` | accounts, users, keys, grants, sessions and the audit log, when accounts are on (SQLite, WAL mode) |
| `lock` | held by the running server; its pid, address and public URL, which `zsql server status` reads |

A deploy is staged beside the branch and swapped in by rename, so a
reader of the directory sees a branch as it was or as it is, never half
written. A restart reloads every branch from disk in well under a
second; sessions survive, because they are in SQLite, and the in-memory
rate limit buckets reset.

## What it is told

`ZSQL_*` variables are the base and flags win over them; the library
itself reads no environment, so a flag and a variable can only mean the
same thing. The ones an operator sets:

| variable | what |
|---|---|
| `ZSQL_BIND` | the address; `127.0.0.1:3699` by default, and a proxy in front is the usual arrangement |
| `ZSQL_DATA_DIR` | the data directory |
| `ZSQL_ACCOUNTS=on` | multi-tenant: keys, accounts, sign-up; `ZSQL_SIGNUP=closed` once the accounts that should exist do |
| `ZSQL_PUBLIC_URL` | where the app is reached from outside: the base of emailed links, what marks session cookies Secure under `https://`, and the one origin a browser may speak MCP from |
| `ZSQL_TRUSTED_PROXY=1` | read the caller's address from `CF-Connecting-IP` or the rightmost `X-Forwarded-For` hop rather than the socket; only safe when the port is reachable by the proxy alone |
| `ZSQL_MAIL_COMMAND`, `ZSQL_MAIL_FROM` | how verification and reset links go out, see below |
| `ZSQL_LIMITS`, `ZSQL_LIMIT_*` | the rate limits; `docs/rate-limiting.md` |
| `ZSQL_MAX_BRANCHES_PER_PROJECT`, `ZSQL_MAX_BRANCHES_PER_ACCOUNT` | the branch caps, 5 and 20; `off` for neither |

`ops/zsqld.service` is a systemd unit to start from: a dedicated user, a
state directory systemd owns, no capabilities, a read-only filesystem
apart from that directory, a private temp directory and a system call
filter. The `ZSQL_*` variables go in its `EnvironmentFile`, mode 0600.

## The first lines it logs

```
zsqld: 9 deployment(s) loaded from /var/lib/zsqld; accounts on, sign-up open; rate limits on; mail on
zsqld: branch caps: 5 per project, 20 per account
zsqld: listening on http://127.0.0.1:3699
```

After a restart, compare the first line with the previous one: the
number of deployments, and whether accounts, sign-up, limits and mail
are as before. A branch this build cannot load is logged by name with
its reason, left out, and counted in a line after the summary saying
what is not being served; everything else comes up. The fix for one is
to deploy that branch again from its repository, since the compiled
model is never migrated.

## Mail

The server has no SMTP client. It writes each message to
`ZSQL_MAIL_COMMAND` on stdin, the way `sendmail` has always been fed
(`msmtp -t`, `sendmail -t`, a script), with `ZSQL_MAIL_FROM` as the
sender and `ZSQL_PUBLIC_URL` as the base of the link. No provider is
compiled in, no credential is in the server's environment, and changing
provider is a config file rather than a release. With no command
configured, links are written to the log instead and never into a
response, which is how a development server is used without mail. The
startup line says which. A command that fails or stops reading is
logged with its exit and what it said; nothing watches that line for
you.

The password reset route is a mail-sending primitive anyone can reach.
The per-address `reset` gate and the one-link-per-user-per-minute rule
stand between a stranger and the provider's daily quota.

## Backups

Back up the data directory as a whole: one archive of the database
snapshot and the deployed-model tree is one restore point. Never copy
`accounts.db` as a file: in WAL mode the file at rest can be a few
kilobytes while its write-ahead log holds everything. Ask SQLite for a
consistent copy instead, and check it before keeping it:

```
sqlite3 /var/lib/zsqld/accounts.db "VACUUM INTO '/tmp/accounts.db'"
sqlite3 /tmp/accounts.db "PRAGMA integrity_check"
```

The server keeps serving throughout. Restoring is stopping the server,
putting the directory back, fixing ownership and starting it; a restore
is worth rehearsing once by extracting into an empty directory, starting
a server on it, signing in and planning a query.

## What changes shape between releases

**`accounts.db`.** `MIGRATIONS` in `crates/accounts/src/lib.rs` is a
list of numbered steps and `PRAGMA user_version` holds how many of them a
file has run, so the length of the list is the schema version. Opening
a database applies the steps it has not seen, each committed together
with the version it reaches, so an interrupted upgrade leaves the file
on the step before rather than halfway through one. The list is append
only; a shipped step is never edited. A file whose version is higher
than the build knows is refused, as is one with tables but no version.

A release that adds a step migrates the database on the first restart,
by whichever binary starts first. Take a backup before that restart and
read the first lines after it. A step that fails leaves the file on the
version before it and the unit dead, not serving something half changed.

**The compiled model.** `snapshot.json.gz` carries a `version`, and
`zsql_semantic::snapshot::FORMAT` is the newest a build writes and
reads. Nothing migrates a snapshot and nothing needs to: it is compiled
from a project in somebody's repository, so the fix for one a build
cannot read is to deploy that branch again, and startup says which.

## Memory

A deployed model is held in memory for the life of the process and
nothing evicts it: about 5 MB for a model of the TPC-DS sample's size.
The branch caps bound one account; they do not bound the machine across
many, and a process killed for memory restarts into the same reload.
Size the machine for the accounts it serves, or set `MemoryMax` and
watch the journal. Lazy loading with an LRU is the open design here;
`docs/accounts-and-access.md` parks it with the multi-instance work.

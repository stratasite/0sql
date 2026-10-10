# Running it

What is deployed, how it gets there, and what is left to do. Written
2026-10-02, when the service first went up.

## Where it runs

`buzz`, a Debian 13 VPS with 4 cores and 16 GB, reachable on the tailnet
as `buzz` and publicly as **https://app.0sql.io**. It also runs the
license server and several containers; zsqld is the only thing installed
directly on the host.

| | |
|---|---|
| binary | `/usr/local/bin/zsqld` |
| settings | `/etc/zsqld.env`, mode 0600 |
| unit | `/etc/systemd/system/zsqld.service` |
| data | `/var/lib/zsqld`, owned by the `zsqld` system user |
| listening | `127.0.0.1:3699`, nothing else |

The unit runs with no capabilities, a read-only filesystem apart from
its own state directory, a private temp directory, a system call filter
and network limited to IPv4, IPv6 and Unix sockets. `StateDirectory`
means systemd creates and owns the data directory, so there is no
ownership to get wrong by hand.

Cloudflare terminates TLS and a token-managed `cloudflared` reaches the
service over the loopback. Because the tunnel is token-managed, the
public hostname lives in the Cloudflare dashboard, not in a file on the
box. Two settings depend on that arrangement: `ZSQL_PUBLIC_URL` is what
marks session cookies Secure, and `ZSQL_TRUSTED_PROXY=1` is what makes
the server read the caller's address out of a header instead of seeing
cloudflared's loopback connection for everyone. Both are set.

Verified when it went up: the client address really does arrive. The
loopback login bucket was spent from on the box until it refused, and a
request through Cloudflare immediately afterwards got its own allowance.

## Releasing

Two tags, deliberately separate. The server ships to one machine we run;
the CLI ships to everyone else.

| what | tag | result |
|---|---|---|
| server | `v0.1.1` | one static binary on the private release |
| CLI | `cli-v0.1.1` | four binaries, private release and public |

Both read the version from the workspace manifest and the workflow
refuses a tag that disagrees with it. Because the workspace shares one
version, a CLI-only fix still means bumping the number the server
carries. Worth revisiting if the two start moving at different rates.

Tests are not run in CI by choice; they run locally before tagging.

Both builds are musl rather than glibc. For the server that is about
portability. For the CLI it is sharper: it resolves the server's
hostname, and a statically linked glibc cannot, because it loads its
name service modules at runtime and dumps core without them. That was
observed, not assumed.

### Releasing the server, in order

`.github/workflows/release.yml` builds on a `v*` tag. It is not the CLI's
workflow and does not share a step with it: no public repository, no
`CLI_RELEASES_TOKEN`, one binary rather than four. A `cli-v*` tag does
not trigger it and a `v*` tag does not trigger the CLI's.

1. **Bump the version in `Cargo.toml`.** `[workspace.package]`, which is
   the first `^version` line in the file and the one CI compares a tag
   against. The whole workspace shares it, so a server-only change still
   moves the number the CLI carries.

2. **Refresh `Cargo.lock` and commit it with the bump.** Every workspace
   crate appears in the lock file with its version, one line each, and
   the build runs `cargo build --locked`, which fails rather than
   updating it. `cargo check -p zsql-server` is enough
   to rewrite it. This is the step that is easy to miss, because nothing
   locally complains until CI does.

3. **Run the tests.** CI does not; that is deliberate, and it means
   nothing stands between a broken commit and a tag except this step.
   `cargo test` for the workspace.

4. **Tag and push the tag.** `git tag v0.2.2 && git push origin v0.2.2`.
   Pushing the branch does not build anything; only the tag does. The
   workflow checks the tag against the manifest and stops if they
   disagree, so a forgotten step 1 costs a tag rather than a bad release.

5. **Let the workflow prove the binary before trusting it.** It asserts
   the result is really static, with no dynamic dependencies, then starts
   it and waits for `/healthz`. If either fails there is no release, by
   design. It publishes `zsqld-x86_64-linux` and a `.sha256` beside it on
   a release of that tag, with generated notes. The repository is
   private, so the assets are too.

6. **Install it on buzz, keeping the one it replaces.**

   ```
   gh release download v0.2.2 -p 'zsqld-x86_64-linux*'
   sha256sum -c zsqld-x86_64-linux.sha256
   ssh root@buzz 'cp /usr/local/bin/zsqld /usr/local/bin/zsqld.prev'
   scp zsqld-x86_64-linux root@buzz:/tmp/zsqld
   ssh root@buzz 'install -m755 /tmp/zsqld /usr/local/bin/zsqld && systemctl restart zsqld'
   ```

7. **Read the first lines it logs.** `journalctl -u zsqld -n 10`. The
   startup line says how many deployments loaded, whether accounts and
   sign-up are on, the rate limit mode and whether mail is configured —
   and, when there is one, the line naming deployments that could not be
   loaded and are not being served. That line is the point of reading
   the journal rather than only checking that the unit is active.

   Rolling back is `install -m755 /usr/local/bin/zsqld.prev` over it and
   another restart.

A restart reloads deployments from disk in well under a second, sessions
survive because they are in SQLite, and the in-memory rate limit buckets
reset.

**A schema change makes the order matter.** The binary brings the
migration steps with it, so the database is upgraded by the restart in
step 6, by whichever binary starts first. Take a backup before that
restart rather than relying on the six-hourly one
(`/usr/local/bin/zsqld-backup` writes a restore point on demand), and
read step 7's output before walking away. A step that fails leaves the
file on the version before it, and the unit dead rather than serving
something half-changed.

### Sign-up notifications

`ops/signup-watch/` holds a script, a service and an hourly timer that
are installed on buzz (`/usr/local/bin/zsqld-signup-watch`, units in
`/etc/systemd/system/`). Each hour the script opens the accounts
database read-only, compares the highest user and account ids with
what it reported last time (`/var/lib/zsqld-signup-watch/last`), and
when either has moved mails the new rows to `ajo@strata.do` through
the same `ZSQL_MAIL_COMMAND` the service uses, loaded from
`/etc/zsqld.env`. A quiet hour sends nothing; a failed send leaves the
watermark alone so the next hour retries. The first run only records
where it started. Installed 2026-10-06; `journalctl -u
zsqld-signup-watch` shows each run, and `echo "0 0" >
/var/lib/zsqld-signup-watch/last` followed by `systemctl start
zsqld-signup-watch.service` forces a full report as a test.

### Building without releasing

`release.yml` also has `workflow_dispatch`, which builds and attaches
the binary to the run's artifacts without cutting a release or needing a
tag — so it skips the version check too. That is the way to get a binary
from a branch, or to try the pipeline after changing it:

```
gh workflow run release.yml --ref <branch>
gh run watch
gh run download <run-id>
```

Useful, and worth being careful with: a binary from a branch carries
whatever version the manifest said, so two different builds can call
themselves the same thing. Fine for trying something on buzz, wrong for
leaving installed there.

### The CLI's public half

The 0sql repository is private, and release assets inherit that, so an
anonymous installer cannot read them. The CLI builds are copied to
**stratasite/0sql-cli**, a public repository holding releases and a
README and nothing else. The workflow needs `CLI_RELEASES_TOKEN`, a
fine-grained token with Contents write on that repository alone, because
a workflow's own token only reaches the repository it runs in. The token
expires; a release will fail with a 403 on that day.

`https://0sql.io/install.sh` lives in the marketing repository under
`public/`. It reads the permanent `releases/latest/download/` redirect,
checks the download against `SHA256SUMS`, and refuses rather than
installing anything it cannot verify.

That redirect ignores prereleases and returns 404 for them, so anything
published as a prerelease is invisible to the installer.

## Backups

`/usr/local/bin/zsqld-backup`, run by `/etc/cron.d/zsqld-backup` at 17
minutes past every sixth hour, configured by `/etc/zsqld-backup.env`
(mode 0600). Output goes to the journal under `zsqld-backup`.

Each run writes one gzipped tar holding a consistent database snapshot
and the whole deployed-model tree, so one object is one restore point.
It lands under `zsqld/history/YYYY/MM/DD/` and is also copied to
`zsqld/latest.tar.gz`. Those are kept apart so a lifecycle rule that
expires `history/` can never delete the object a restore starts from.

The database is never copied as a file. At rest the live file can be a
few kilobytes while its write-ahead log holds everything: on the copy
used for testing, 4 KB against 457 KB. `VACUUM INTO` asks SQLite for a
consistent snapshot instead, which also compacts it and leaves no `-wal`
beside it. Every run checks the result with `PRAGMA integrity_check`
before uploading, because a corrupt snapshot that reports success is
worse than a failure.

Nothing is kept on the box. The archive is built in a temp directory and
deleted on exit.

Restoring, which has been done end to end rather than just written down:

```
systemctl stop zsqld
aws s3 cp s3://BUCKET/zsqld/latest.tar.gz - | tar -xzf - -C /var/lib
chown -R zsqld:zsqld /var/lib/zsqld
systemctl start zsqld
```

The archive holds a single `zsqld/` directory, so it unpacks over
`/var/lib` cleanly. A restore was verified by extracting into an empty
directory, starting a server on it, signing in with the original
password, and planning a query against the restored model.

## Done since it went up

### Schema changes, and the version that records them

Two things on disk change shape as the service does: `accounts.db`, and
the compiled model under each branch. Both now say which shape they are
in, which they did not before.

**`accounts.db`.** `MIGRATIONS` in `crates/accounts/src/lib.rs` is a list
of numbered steps and `PRAGMA user_version` holds how many of them a file
has run, so the length of the list is the schema version. Opening a
database applies the steps it has not seen, each committing together with
the version it reaches, so an interrupted upgrade leaves the file on the
step before rather than halfway through one.

The list is append-only. A step that has shipped is never edited: a
database that already ran it will not run it again, so editing one
changes what new files get and nothing else — which is the divergence the
version is there to prevent. Correcting a shipped step means adding
another one.

This replaced a schema replayed with `CREATE TABLE IF NOT EXISTS` on
every open plus a separate list of `ALTER`s for files that predated a
column. That arrangement needed every change written in two places and
had no way to say what shape a file was in; a fresh database and an
upgraded one agreed only as long as both were remembered. They now agree
because both ran the same list.

Two refusals rather than guesses: a file whose version is higher than
this build knows, and a file with tables but no version, which cannot be
told from one halfway through step 1.

The second is what every database from before this change looks like,
including the live one. Its only difference from what step 1 builds is
that `users` gained `email_verified_at` from an `ALTER`, so the column
sits at the end rather than inline — harmless, because every read names
its columns, but it leaves one file that does not match the steps said to
have built it. Bringing it onto the lineage is a rebuild of that one
table and a stamp, with zsqld stopped and after a backup. Every row is
kept, hashes and sessions included, so nobody signs in again:

```
PRAGMA foreign_keys = OFF;
BEGIN;
CREATE TABLE users_new (
  id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, name TEXT NOT NULL DEFAULT '',
  password_hash TEXT, email_verified_at TEXT,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')));
INSERT INTO users_new (id, email, name, password_hash, email_verified_at, created_at)
  SELECT id, email, name, password_hash, email_verified_at, created_at FROM users;
DROP TABLE users;
ALTER TABLE users_new RENAME TO users;
PRAGMA user_version = 1;
COMMIT;
```

Then `PRAGMA integrity_check` and `PRAGMA foreign_key_check` before
starting it again. A `VACUUM INTO` copy taken while it was stopped is
what to put back if either complains.

A later step that has to rebuild a table — SQLite cannot alter a CHECK
constraint, so changing `role IN (...)` or a `level` means create, copy,
drop, rename — needs `foreign_keys` off while it runs, and that pragma
does nothing inside a transaction. It has to be set around the call, not
within a step.

**The compiled model.** `snapshot.json.gz` now carries a `version`, and
`zsql_semantic::snapshot::FORMAT` is the newest this build writes and
reads. Raise it in the same change that makes a snapshot unreadable to
the number before it: a field added without a serde default, or one
renamed or removed, since every struct in that file is
`deny_unknown_fields`. Adding a field that defaults is not such a change.

Nothing migrates a snapshot, and nothing needs to. It is compiled from a
project archive that lives in somebody's repository, so the fix for one
this build cannot read is to deploy that branch again. What changed is
that startup says so instead of refusing to run: see below.

### Startup comes up without what it cannot load

`reload` used to stop at the first branch that would not load, and the
error reached `main`, so one unreadable file meant the service did not
start at all — five restarts in ten seconds and then systemd leaving it
failed. A release that changed the snapshot format without raising
`FORMAT` would have done exactly that, on a machine that was healthy a
moment earlier.

Each branch now loads on its own. One that fails is logged by name with
its reason, left out, and counted in a line after the startup summary
saying what is not being served. Everything else comes up.

This is the first of the two candidates the open item below used to list,
and it only answers the format half. A process that runs out of memory
still restarts into the same reload; that is still open.

### A cap on deployed branches per account

Deployed models are held in memory for the life of the process and
nothing ever evicts them: they are inserted on deploy and at startup,
and removed only when someone deletes a branch or a project.

Measured: about 5 MB per model of the TPC-DS sample's size, 4.2 MB idle,
against roughly 11 GB free on buzz. The deploy allowance is 10 a minute,
so one account could add around 600 models an hour, near 3 GB an hour.

**The cap.** An account may keep 5 deployed branches per project and 20
in all. Going over answers `409 OverLimit` naming what to remove.
Checked before the archive is unpacked, so refusing costs nothing;
counted from the models in memory rather than the project records,
because memory is the thing being protected. Redeploying a branch that
is already deployed is not a new branch, so an account at its cap can
still ship to what it has, and `validate` is exempt since it keeps
nothing. `ZSQL_MAX_BRANCHES_PER_PROJECT` and
`ZSQL_MAX_BRANCHES_PER_ACCOUNT` change them, `off` removes them, and
both are logged at startup. Off entirely when accounts are off: a
single-tenant server is one operator on their own machine.

It bounds one account, not the machine across many; what startup should
do when it cannot load everything is still open, below.

### Password reset, and email verification

Both are live; `accounts-and-access.md` has the model, the token
lifetimes and why a reset answers the same whatever it finds. The
operational half is here.

**Delivery is a command, not an SMTP client.** zsqld writes the message
to `ZSQL_MAIL_COMMAND` on stdin, the way `sendmail` has always been fed.
Nothing about a provider is compiled in: no TLS stack in a binary built
static against musl, no credential in this process's environment, and
changing provider is a config file rather than a release.

On buzz that command is msmtp against the Workspace account on
`strata.do`:

```
# /etc/zsqld.env
ZSQL_MAIL_COMMAND=msmtp -t
ZSQL_MAIL_FROM=0sql support <0sql-support@strata.do>
ZSQL_PUBLIC_URL=https://app.0sql.io
```

```
# /etc/msmtprc, mode 0600, owned by the user zsqld runs as
defaults
tls on
tls_trust_file /etc/ssl/certs/ca-certificates.crt
account gmail
host smtp.gmail.com
port 587
auth on
user <the workspace user>@strata.do
passwordeval cat /etc/zsqld/app-password
from 0sql-support@strata.do
account default : gmail
```

Four things about that account, each of which will bite if missed:

- **An App Password authenticates as a user, never as an alias.** If
  `0sql-support@strata.do` is an alias on an existing user, `user` is
  that user's own address and `from` is the alias; Gmail allows a From
  that is a verified alias of the authenticated account and silently
  rewrites it to the authenticated address if it is not. A real user of
  its own, with its own App Password, costs a seat and needs no alias.
- **App Passwords need 2FA on that user, and a Workspace admin can
  disable them for the whole organisation.** Check both before blaming
  the config.
- **DKIM is not on by default.** Generate the key under Admin → Apps →
  Google Workspace → Gmail → Authenticate email and publish the TXT
  record, or mail is not signed as `strata.do` and the alignment that
  makes this better than a personal Gmail is not there. SPF should
  include `_spf.google.com`.
- **The quota is about 2,000 messages a day**, and a reset endpoint is
  a mail-sending primitive anyone can reach. The `reset` gate (five per
  address, ten an hour) and the one-link-per-user-per-minute rule are
  what stand between a stranger and that allowance.

Mail arrives from `strata.do` while the product is `0sql.com`. Fine for
deliverability, since alignment is with the sending domain; a `0sql.io`
sender later is DNS and `msmtprc`, not a release.

**With no command configured** the links are written to the journal
instead, which is how a self-hosted or development server is used
without mail. They are never put in a response: a reset is asked for by
whoever typed the address, who may not own it. `zsqld` says which mode
it is in on the first line it logs.

**Nothing watches whether mail is actually going out.** A failed send is
one journal line, next to the backup failures that are also unwatched.

### The MCP endpoint, and the context bound to a grant

`POST /mcp` serves the reads as tools to an agent holding a key;
`docs/mcp.md` has the design and the rules. Operationally it is nothing
new: the same binary, the same unit and port, the same tunnel, no
setting. Two things to know on the box:

- **Migration step 3** adds `context` to `query_key_grants`, so the
  release that carries it is one of the restarts that wants a backup
  first, per the order above. Existing grants come up unbound, which is
  the behaviour they had.
- **`ZSQL_PUBLIC_URL` is now also the one Origin a browser may speak
  MCP from.** It is set on buzz. A server without it falls back to its
  bind address, which refuses every browser origin but its own.

Checking a release from the outside, with a query key granted one
branch:

```
curl -si -X POST https://app.0sql.io/mcp -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}'
```

answers 401 with `WWW-Authenticate: Bearer realm="0sql"`, which is the
line to read: it proves Cloudflare passes the challenge through. With
`-H 'authorization: Bearer zqk_...'` the same call answers the server's
name and version, and then

```
claude mcp add --transport http 0sql https://app.0sql.io/mcp \
  --header "Authorization: Bearer zqk_..."
```

gives Claude Code the tools; `list_projects` should show exactly the
granted branch.

## Open

### Startup when it cannot fit everything

Skipping what will not load, above, removed the restart loop for a model
that cannot be *read*. It does nothing for one that does not *fit*: a
process killed for memory is killed while loading, not handed an error it
can skip, so it restarts, reloads the same models and is killed again.
After five attempts in ten seconds systemd gives up. `MemoryMax` is
unset.

The branch caps bound one account, not the machine across many: twenty
models is about 100 MB, so a hundred accounts is 10 GB, which is the
whole box. The caps buy room, they do not remove the ceiling, and they
do nothing about this failure mode.

What is left of the answer is lazy loading with an LRU, which
`accounts-and-access.md` already parks for the multi-instance work.

### Smaller

- **No retention rule on the backup bucket.** Four objects a day
  accumulate. The rule belongs on `zsqld/history/`, never on the whole
  prefix. If the bucket has versioning, `latest.tar.gz` is overwritten
  four times a day and wants a noncurrent-version rule too.
- **Nothing watches the backups.** Failures reach the journal and
  stop there. There is no mail on the box.
- **A `rust-toolchain.toml` that only CI honours.** The development
  machine uses a distribution Rust rather than rustup, so the pinned
  version governs releases only.
- **Two GitHub Actions are a major version behind** and are being forced
  onto a newer Node. Harmless, since the binaries are static.

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

Deploying a new server build is three steps: tag, take the binary from
the release, `install -m755` it over `/usr/local/bin/zsqld` and
`systemctl restart zsqld`. Keep the previous binary beside it to roll
back. A restart reloads deployments from disk in well under a second,
sessions survive because they are in SQLite, and the in-memory rate
limit buckets reset.

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

## Open

### A cap on deployed branches per account

The one with teeth. Deployed models are held in memory for the life of
the process and nothing ever evicts them: they are inserted on deploy
and at startup, and removed only when someone deletes a branch or a
project. Nothing caps how many branches an account may deploy.

Measured: about 5 MB per model of the TPC-DS sample's size, 4.2 MB idle,
against roughly 11 GB free on buzz. The deploy allowance is 10 a minute,
so one account can add around 600 models an hour, near 3 GB an hour.

What makes it more than a slow leak is the restart behaviour. Startup
reloads every deployment from disk, so once memory is exhausted the
service is killed, restarts, reloads the same models and is killed
again. After five attempts in ten seconds systemd gives up and leaves it
failed, with the thing that brought it down still on disk waiting to be
reloaded. `MemoryMax` is unset.

So two decisions, not one: a limit on branches per account, and what the
server should do when it cannot load everything at startup. Lazy loading
with an LRU, which `accounts-and-access.md` already parks for the
multi-instance work, would answer the second.

This is reachable by any single account, so it is not an argument about
sign-up.

### Password reset, and email verification

Already parked in `accounts-and-access.md`. Both matter more now the
service is public and anyone can sign up.

Without reset, a user who forgets their password has no path back
without someone editing SQLite by hand. Without verification, an account
can be made with an address its owner does not control, so the email
cannot be trusted for support or billing.

Neither is a reason to close sign-up: closing it helps nobody who is
already locked out, it only limits how many people meet the gap.
Sign-up being open is a deliberate choice, since self-serve is the
point of a multi-tenant service.

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

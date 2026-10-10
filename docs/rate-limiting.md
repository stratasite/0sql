# Rate limiting

Decided and implemented 2026-10-02. The design was reviewed as a proposal
first; this is that document updated to what the code does, with the
numbers measured rather than guessed.

## What saturated the server

zsqld is one process. Every request ran on the tokio runtime, and every
handler was synchronous inside its `async fn`: nothing was moved to a
blocking thread. The costs, measured on a Ryzen 5 7640U (12 threads),
release build, against the TPC-DS sample:

| work | cost | where |
|---|---|---|
| plan (`sql`, `explain`) | ~70 µs | `plan_request` |
| explore, fields search | a walk or a trigram scoring pass over every visible field; milliseconds, grows with the model | `explore`, `fields` |
| deploy, validate, test | untar, load YAML, build the universe and the per-root index, run the tests; seconds for a real model | `deploy_or_validate` |
| login, sign-up, invite accept, set password, accept a reset | one argon2 hash each, tens to hundreds of ms, before any identity is known | `login`, `signup`, `accept_invite`, `set_password`, `accept_reset` |
| ask for a reset, resend a verification | a subprocess that talks to a mail relay, and a message against a finite daily allowance | `request_reset`, `resend_verification` |
| any authenticated request | SHA-256 of the key plus two or three SQLite lookups under one store mutex | the `Auth` extractor, `gate` |
| any body | the 256 MB limit was on the whole router, not just deploy | `main` |

Three ways to saturate it, in order of ease:

1. **Unauthenticated argon2.** A loop of `POST /login` with any password
   pinned a core per request with no key needed. The cheapest attack, and
   also what a credential-stuffing bot does by accident.
2. **Concurrent deploys.** A handful of deploys in flight occupied that
   many runtime workers for seconds each, stalling every other request.
3. **Read volume from one tenant.** Plans are microseconds, so this needs
   real volume, but one misbehaving application could take every core
   from every other account.

And a ceiling that was not where it looked: with accounts on the server
topped out at **13,300 requests a second while using under three of
twelve cores**. The store mutex, not the planner, was the limit.

## Principles

- Protect the box first, then share it fairly. Admission control keeps
  the process alive; per-tenant limits keep one account from crowding out
  the rest. Both are needed; neither alone is enough.
- Price work by what it costs, not by the request count. One deploy is
  not one `sql`.
- Everything is in memory in this one process, like the deployments.
  Limits reset on restart, which is fine. A limiter shared between
  instances belongs with the storage trait that multiple instances will
  need anyway (see `accounts-and-access.md`), not here.
- Answer in a shape the CLI and the app already understand, and say when
  to come back.
- Off by default when accounts are off, apart from the admission caps and
  the body limits, which protect a self-hosted box just as well.

## Step zero: authentication off the hot path

Before limiting anything it was worth removing the bottleneck the limits
would otherwise be protecting. Every request with a key asked the store
the same three questions, each under the one `Mutex<Connection>`: who is
this, what project, what level. The answers change only when someone
changes them.

`Store` now keeps those three answers in memory (`crates/accounts`):
principals by secret hash, sessions by token hash, projects by uid, and
levels by project and user. The TTL is 60 seconds (`ZSQL_AUTH_CACHE_TTL`,
`0` to turn it off), but the TTL is only a backstop. Correctness rests on
two things:

- **Every change empties the cache.** The audit row that every account
  change writes is the one place this happens, so a new kind of change
  cannot forget to; logout and session deletion, which write no audit
  row, call it directly. Emptying all of it rather than the entries that
  changed is what keeps "the old secret stops at once" true without
  tracking which decision depended on what. Changes are rare.
- **A read that raced a change does not cache what it saw.** Each change
  also moves a generation counter, under the connection lock; a read
  captures the generation under the same lock and only caches if it has
  not moved. This closes the window between a read finishing and its
  answer being stored.

A secret that matches nothing is never cached, so a stream of made-up
keys cannot fill memory. Key use is still recorded about once a minute,
since that is now how often a cache miss happens.

Measured, same machine and model:

| | peak req/s | cores used |
|---|---|---|
| accounts on, before | 13,300 | 2.9 |
| accounts on, after | 42,700 | 7.5 |
| accounts on, after, limits enforcing | 38,900 | 7.4 |

Three and a bit times the ceiling, and the server now uses the machine it
is on. Enforcing the limits costs about 9% of that at full tilt, and
nothing worth measuring at realistic rates.

## The design

### 1. Admission: concurrency caps per cost class

A `tokio::sync::Semaphore` per class, held for the duration of the work.
Acquired with a short timeout; past it the answer is `503 Busy` with
`Retry-After` rather than an unbounded queue.

| class | routes | permits | wait before 503 |
|---|---|---|---|
| heavy | deploy, validate, test, sampling-plan | 2 | 5 s |
| password | login, signup, invites/accept, account/password, the reset and verification routes | 2 | 2 s |
| explore | explore, fields, and the `fields` and `explore` MCP tools | cores | 1 s |
| plan | sql, explain, tables, values, branch summary, projects, `POST /mcp` | cores × 4 | 1 s |
| manage | the account endpoints | cores × 2 | 1 s |

The heavy and password classes also run on `tokio::task::spawn_blocking`,
so a deploy or an argon2 hash never holds a runtime worker. Measured: 508
deploys running flat out during a load test cost plan throughput 20% and
left its p99 at 5.7 ms. Before, deploys stalled the runtime.

Requests also carry a deadline: 90 s for heavy, 30 s for everything
else. A deadline that passes answers `503 Timeout` **without**
`Retry-After`, because unlike the other two refusals it happened after
the handler started.

Ninety rather than the couple of minutes a large deploy might want,
because Cloudflare gives an origin a hundred seconds and then answers
`524` itself. A 524 carries no retry hint, so the CLI cannot act on it
and the caller is left guessing about a deploy that may well have
landed. The server should always get to give its own answer first. On a
deployment without that ceiling in front, this is the number to raise.

### 2. Fairness: token buckets per principal

A bucket per `(account, class)`, and a second per `(query key, class)` so
one application cannot drain its account's allowance from the others. A
request must find a token in both; if the key's bucket refuses, the
account's token is given back.

| class | per account | per query key |
|---|---|---|
| plan | 100/s, burst 400 | 50/s, burst 200 |
| explore | 20/s, burst 60 | 10/s, burst 30 |
| heavy | 10/min, burst 3 | not allowed: query keys are read only |
| manage | 60/min, burst 120 | n/a |

An MCP request is one tool call and is charged as a plan by the guard;
the two explore-class tools take an explore token and seat as well,
inside the dispatcher, so an agent exploring is paced like an
application exploring. See `docs/mcp.md`.

These are fairness knobs, not capacity: 100/s per account is a quarter of
a percent of what one core does. About 130 accounts could run at their
full plan allowance at once on this machine. Raise them freely; the
measurements above say where the real wall is.

The buckets live in 16 sharded maps so the limiter does not become the
next store mutex. Idle buckets are swept every five minutes.

### 3. Unauthenticated routes: per-address limits

| route | limit per address |
|---|---|
| `POST /login` | 10/min |
| `POST /signup` | 5/hour |
| `POST /invites/accept`, `/password-reset/accept`, `/verify-email` | 10/min |
| `POST /password-reset` | 10/hour, burst 5 |
| any 401 answer | 60/min |

Asking for a reset gets its own low rate because it is the one
unauthenticated route that makes the server send mail, and a sender's
daily allowance is a finite thing a stranger can spend. It is paired
with a rule in the store rather than here: one link per user per minute,
whichever address asked, so the address limit stops one person asking
for many and the cooldown stops many addresses mailbombing one victim.
Redeeming a link is grouped with accepting an invite: the tokens are
unguessable, so that limit is about the cost of being asked.

Plus a counter per email: five failed sign-ins and that email is refused
for fifteen minutes whichever address they came from, which is what
someone working through a password list looks like.

**This is the one limit here that a stranger can trip on your behalf.**
Knowing an email is enough to keep that person out of the web app for the
window; their keys keep working, and only the password path is affected.
It is the classic account-lockout trade, taken deliberately, and
`ZSQL_LIMIT_EMAIL_FAILURES=off` turns it off without touching anything
else. Worth revisiting if anyone is annoyed by it.

The address is the socket peer. Behind a proxy that is wrong for every
client, so `ZSQL_TRUSTED_PROXY=1` makes the server read two headers
instead, in order of how far they can be trusted.

`CF-Connecting-IP` first: Cloudflare sets it to the single address that
reached its edge, and being one value it cannot be padded into a list to
hide behind. Failing that, the **rightmost** hop of `X-Forwarded-For`.

That is the opposite of the usual advice and deliberately so. Every
proxy appends, so the leftmost entry is whatever the caller sent and is
forgeable, while the rightmost is the one the nearest proxy wrote
itself. Cloudflare's edge appends the connecting address to any header
already there, and `cloudflared` appends the visitor again on the way to
the origin, so the rightmost is the real visitor there too. The
Cloudflare header is preferred anyway, because that ordering is the
subject of an open issue asking for it to be reversed.

Neither header is read uninvited. That is how per-address limits get
bypassed.

### 4. Shape limits: bound the cost of one request

Rate limits cap how many requests; these cap how big one can be. Each
refusal is a `400 TooLarge` naming the limit.

| | limit |
|---|---|
| deploy and validate body | 64 MB (the TPC-DS sample is 10 KB) |
| every other body | 1 MB (was 256 MB for everything) |
| shorthand expression | 8 KB |
| projections | 100 |
| calculations | 50 |
| segments | 50 |
| filters, and nesting depth | 200, 8 |
| values in one spec, and nesting depth | 20,000, 32 |
| one string in a spec | 64 KB |
| a `q` search term | 200 characters |

### 5. Configuration

```
ZSQL_LIMITS=on|shadow|off       default: on when accounts are on, off otherwise
ZSQL_LIMIT_PLAN=100/s,400       sustained,burst per account; `off` for no limit
ZSQL_LIMIT_EXPLORE=20/s,60
ZSQL_LIMIT_HEAVY=10/m,3
ZSQL_LIMIT_MANAGE=60/m,120
ZSQL_LIMIT_KEY_PLAN=50/s,200    per query key
ZSQL_LIMIT_KEY_EXPLORE=10/s,30
ZSQL_LIMIT_LOGIN=10/m,10        per address
ZSQL_LIMIT_SIGNUP=5/h,5
ZSQL_LIMIT_INVITE=10/m,10
ZSQL_LIMIT_BAD_KEY=60/m,60
ZSQL_LIMIT_EMAIL_FAILURES=5     failed sign-ins per email; `off` to disable
ZSQL_LIMIT_EMAIL_WINDOW=900     seconds
ZSQL_ADMIT_PLAN, _EXPLORE, _HEAVY, _MANAGE, _PASSWORD    concurrency permits
ZSQL_TRUSTED_PROXY=1            read X-Forwarded-For
ZSQL_AUTH_CACHE_TTL=60          seconds; 0 turns the auth cache off
```

`shadow` runs every check and logs what it would have refused, with the
caller and the class, without refusing anything. The way to roll this out
on a server with real traffic is a week in `shadow`, then read the log,
adjust, and switch to `on`. The admission caps, the body limits and the
shape limits do not have a shadow mode and do not need one: they only
change what happens under load that already fails.

### 6. What clients see

`429 RateLimited` and `503 Busy` both carry `Retry-After`, and every
limited answer carries `RateLimit-Limit`, `RateLimit-Remaining` and
`RateLimit-Reset` so a client can pace itself before it is refused rather
than after. The body is the usual shape:

```json
{"error": {"class": "RateLimited", "message": "too many requests: retry in 1s"}}
```

- **The CLI** waits and asks again, up to three times, honouring the
  server's delay up to 30 seconds, and says so on stderr so `--json`
  output stays clean. It retries a `429`, and a `503` that names a delay,
  whatever the command: both mean the server refused before running any
  of the request, so repeating it is safe even for a deploy. A `503`
  without a delay is a deadline that passed inside the handler, which may
  have done part of the work, so that one is reported, not repeated.
- **The web app** shows the message and holds the playground's run
  buttons for the delay, counting down, so a repeated click does not
  spend the next allowance the moment it arrives. The field search
  debounce went from 150 ms to 250 ms, since searching and exploring
  share one allowance.

## Deferred

- A `limits` column on `accounts`, read into the principal at
  authentication, so one customer can be given a bigger allowance without
  a restart. The code takes its rates from one `Config`; this is where
  per-account overrides would go.
- "Throttled N times in the last hour" per query key on the Query keys
  page, which is how an owner would find the application misbehaving.
- A limiter shared between instances, with the storage trait.
- Smarter lockout: counting distinct addresses per email rather than
  failures, which would remove the trade in section 3.

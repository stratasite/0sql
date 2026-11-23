# Admission and request bounds

What the server does to stay alive, and what it leaves to whatever is in
front of it.

The server is one process that answers whoever can reach it. It is
meant to sit behind the developer's own service, the way a database
does, and that service is where callers are identified and paced. So
there is no rate limiting here: no token buckets, no per-address
limits, no `429`. A proxy or the calling service does that, with the
knowledge of who the callers are that this server does not have.

What stays is what a database also keeps for itself: a cap on how much
work runs at once, a deadline on each request, and a bound on how big
one request can be.

## What the work costs

Measured on a Ryzen 5 7640U (12 threads), release build, against the
TPC-DS sample:

| work | cost |
|---|---|
| plan (`sql`, `explain`) | ~70 µs |
| explore, fields search | a walk or a trigram scoring pass over every visible field; milliseconds, grows with the model |
| deploy, validate, test | untar, load YAML, build the semantic model and the per-root universes, run the tests; seconds for a real model |

## Admission: concurrency caps per cost class

A `tokio::sync::Semaphore` per class, held for the duration of the work.
Acquired with a short timeout; past it the answer is `503 Busy` with
`Retry-After` rather than an unbounded queue.

| class | routes | permits | wait before 503 |
|---|---|---|---|
| heavy | deploy, validate, test | 2 | 5 s |
| probe | a datasource's tables and one table's structure: `POST .../datasources/{ds}/tables` and `.../tables/{table}` | cores × 2 | 1 s |
| explore | explore, fields, and the `fields` and `explore` MCP tools | cores | 1 s |
| plan | everything else: sql, explain, tables, values, branch summary, projects, removal, `POST /mcp` | cores × 4 | 1 s |

`/` and `/healthz` take no seat.

The heavy class also runs on `tokio::task::spawn_blocking`, so a deploy
never holds a runtime worker. Measured: 508 deploys running flat out
during a load test cost plan throughput 20% and left its p99 at 5.7 ms.

A probe spends no CPU to speak of and seconds waiting on a warehouse,
so it has seats of its own, and runs on a blocking thread as heavy work
does: a slow warehouse fills the probe seats and leaves planning and
deploys alone. Its statement is given 80 s at most, short of the
deadline below, so a warehouse that is slow is answered with its own
error.

An MCP request is one tool call and is admitted as a plan by the guard;
the two explore-class tools take an explore seat as well, inside the
dispatcher. See `docs/mcp.md`.

A write that lacks the deploy token is refused before it takes a seat,
so it costs nothing and never waits behind a deploy to be told no.

## Deadlines

90 s for heavy and for a probe, 30 s for everything else. A deadline that passes
answers `503 Timeout` **without** `Retry-After`, because unlike `Busy`
it happened after the handler started.

Ninety rather than the couple of minutes a large deploy might want,
because Cloudflare gives an origin a hundred seconds and then answers
`524` itself. A 524 carries no retry hint, so the CLI cannot act on it
and the caller is left guessing about a deploy that may well have
landed. The server should always get to give its own answer first. On a
deployment without that ceiling in front, this is the number to raise.

## Shape limits: the cost of one request

Each refusal is a `400 TooLarge` naming the limit.

| | limit |
|---|---|
| deploy and validate body | 64 MB (the TPC-DS sample is 10 KB) |
| a deploy, decompressed | 256 MB, 20,000 entries |
| every other body | 1 MB |
| shorthand expression | 8 KB |
| projections | 100 |
| calculations | 50 |
| segments | 50 |
| filters, and nesting depth | 200, 8 |
| values in one spec, and nesting depth | 20,000, 32 |
| one string in a spec | 64 KB |
| a `q` search term | 200 characters |

## Configuration

```
ZSQL_ADMIT_PLAN, ZSQL_ADMIT_EXPLORE, ZSQL_ADMIT_HEAVY, ZSQL_ADMIT_PROBE    concurrency permits
```

## What clients see

`503 Busy` carries `Retry-After`. The body is the usual shape:

```json
{"error": {"class": "Busy", "message": "the server is full of heavy work: retry in 5s"}}
```

The CLI waits and asks again, up to three times, honoring the server's
delay up to 30 seconds, and says so on stderr so `--json` output stays
clean. It retries a `503` that names a delay whatever the command: it
means the server refused before running any of the request, so repeating
it is safe even for a deploy. A `503` without a delay is a deadline that
passed inside the handler, which may have done part of the work, so that
one is reported, not repeated. It treats a `429` from a proxy in front
the same way as a `503` with a delay.

## What is not here

Rate limits, per-caller fairness and per-address limits were part of
the server while it had accounts, and left with them. A server exposed
to callers it does not trust needs them from its proxy: admission keeps
the process alive under a flood, but it does not keep one caller from
taking every seat.

Nothing evicts a deployed model, and nothing caps how many are
deployed: whoever can deploy can fill memory. `docs/operating.md`
covers sizing.

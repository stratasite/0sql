# SQL in the CLI, and the server inside it

The plan for bringing SQL generation back into `zsql`, with a `-r` flag
for the server, and for running the server from the same binary. Written
2026-10-15. The phases at the end are the checklist; tick them off here.

## Why

The CLI was made thin on 2026-10-01 so the engine never left the service.
The whole thing is going open source, so that reason is gone, and a
developer with a checkout should be able to plan SQL, run tests and
explore a model without a server or a network. The code that does this
already lives in library crates; only the server wraps it. The work is to
lift that wrapper into a crate both binaries share.

## Measurements the plan rests on

Release build, this machine, largest local project (34 tables, 353
fields, 1,253 paths, about 100 KB of YAML):

| step | time |
|---|---|
| bare `zsql` process start | 5 to 6 ms |
| YAML load, formation, planner index | 12 to 15 ms |
| same model from a compiled snapshot (30 KB gzip) | 4 to 5 ms |
| lower one spec, then plan it (bench average over goldens) | 21 us, then 74 us |

A local `zsql sql` lands around 20 ms end to end. A snapshot cache would
save roughly 10 ms of that, which is why the cache is deferred behind
measured budgets rather than built first.

## Shape

### One new crate, `crates/service` (`zsql-service`)

The word "engine" stays reserved for the planner, as the README uses it.
The service crate is the layer between a loaded model and a caller,
lifted out of `server/src/main.rs` without behaviour change:

- `Model`: uid, branch, the self_cell pinning `Universe` to `Planner`,
  sampled values, tests, warnings. Built by `Model::from_dir(dir, Trust,
  ...)` and `Model::from_snapshot(...)`. `Trust` is an enum, `Owned` or
  `Untrusted`, not a bool: the server passes `Untrusted` and gets the
  confined loader, the CLI passes `Owned`. A compile-time assertion in
  the crate's tests pins `Model: Send + Sync`, so an `Rc` added in a
  refactor fails there and not inside axum.
- The request and response types and the functions behind every read:
  `sql`, `explain`, `explore`, `fields`, `tables`, `values`, `run_tests`,
  `summary`, `spec_of`. They take `&Model` and an
  `Option<SecurityContext>`. Dialects stay `&'static`, leaked once per
  process in each binary, and live on `Model` so the functions do not
  take them separately.
- `ServiceError { class, message, kind }` where `kind` is `Invalid`,
  `Unprocessable`, `NotFound` or `Internal` (the server's request-shape
  limits make their own `TooLarge`). The server maps
  the kind to a status; the CLI prints `class: message`, which is what it
  already prints for a server refusal. The `ContextRequired` rule lives
  here so local planning enforces it. The request shape limits stay in
  the server as a service policy (`docs/rate-limiting.md`).
- Every crate gets `publish = false` until the API has had a second
  caller for a while. The path dependencies are the contract for now.

### The server becomes a library plus a thin binary

`zsql_server::run(Config)` takes one struct: bind, data dir, accounts on
or off with sign-up, public URL, trusted proxy, limits, admission, caps,
mail and MCP settings. the library makes zero `std::env::var` calls, enforced by
a unit test that greps its own source (phase B moved the one place that
reads `ZSQL_*` variables into the CLI's serve module, and `zsqld` went). The `OnceLock` holding `ZSQL_MCP_TOOLS`
becomes a field on `App`, so two servers can start in one test process.
This is the widest part of the refactor and the reason the
server-in-CLI work is its own phase.

### CLI rules, written into the help text

Anything that needs only the model runs here; anything about the
server's copy needs `-r`.

- `-r, --remote` is global. `--server` and `--uid` imply it.
  `ZSQL_SERVER` in the environment does not. `--branch` without remote
  is an error that says to add `-r` or check the branch out.
- Local by default: `sql`, `explain`, `explore`, `fields`, `tables`,
  `values`, `check`, `test`, `repl`. Always remote: `deploy`, `status`,
  `list`, `remove`, `health`, `auth`.
- Local mode never runs git. `Where::resolve` splits into a local
  target, which needs only the project directory, and a remote one,
  which is the only place the branch is read.
- Errors: without `--json`, `class: message` on stderr and exit 1. With
  `--json`, `{"error":{"class":...,"message":...}}` on stdout and exit 1.
  Usage errors exit 2. Identical for local and remote, since the remote
  path already parses that shape.
- The shorthand `expr` is handed to the service, not parsed in the CLI,
  so item descriptions and corrections come from one code path.
- `remote.rs` splits into `remote.rs` for HTTP and `show.rs` for
  printing over the shared JSON shapes. The footer says where the plan
  was made.
- `main` decides the SIGPIPE disposition per command. Commands that
  write to stdout reset it to default as today; `serve` and `server
  start` leave Rust's ignore in place, because a write to a closed
  socket would otherwise kill the server.
- `ZSQL_HOME` overrides `~/.zsql` for history, data, pidfile and log.
  Every test that opens the repl or starts a server sets it to a temp
  directory.

### The repl holds the model

Load once and keep it. The loader gains `Loaded.sources`, the list of
every file it read including imports from outside the tree. Before each
query the repl stats that list and reloads when any size or mtime moved,
printing `reloaded in N ms`; a failed reload keeps the old model and
prints the error. `.reload` forces it. With `-r` the repl is unchanged.

### Loader efficiency, measured before cached

Budget on the TPC-DS fixture: local `zsql sql` under 25 ms end to end,
repl reload under 15 ms. `zsql-dev load-bench --project DIR` reports
the loader step by step (`Loaded.phases`), which is what decided the
work below. A compiled snapshot cache under `$XDG_CACHE_HOME/zsql/<uid>/`,
keyed on `Loaded.sources` plus the snapshot format, is built only if the
budgets are missed.

What the measurement said, release build, TPC-DS fixture (33 tables,
380 fields, 184 KB of YAML), per load:

| step | before | after |
|---|---|---|
| table files parsed (YAML) | 2.5 ms | 0.9 ms, on threads |
| fields built from them | 5.1 ms | 1.5 ms |
| relationships, formation, post-universe, security, tests | 1.2 ms | 1.2 ms |
| universe index, planner index | 0.6 ms | 0.6 ms |
| whole load | 9.6 ms | 5.0 ms |
| `zsql sql` end to end (wall clock) | 15 ms | 8 to 9 ms |

What the plan guessed at and the numbers dismissed: `LoadOptions` to
skip tests and values (32 us and 3 us), the second `Universe::build`
inside formation and the snapshot clone (0.15 ms and 0.12 ms). Neither
is worth an API. What was actually slow: the dialect's reserved-word
and aggregate lists were rebuilt, hundreds of allocations each, for
every one of 380 fields (now once per datasource); every YAML lookup
allocated a key (now `&str`); each file's mapping was cloned on the way
in (now moved); and the table files were parsed one at a time (now
prefetched on up to eight threads, each outcome handed to the ordered
read that would have produced it, so errors name the same file as
before). The cache is not built: the budget is met three times over.

### Server in the CLI, embedded

Decided 2026-10-15, after the 0.6.0 PR: the embedded server is the
whole server, accounts included. The tool exists so developers can
deploy a service their own applications call, and that service needs
keys, the MCP endpoint and mail; a single-tenant stand-in would be the
thing everyone learns and then has to leave. So `zsql serve` is the
server, and SQLite (the only C in the build) comes
into the CLI, which the server's musl build already proves. An MCP
server over stdio for Claude Code is parked: the apps talk HTTP, and
the HTTP MCP endpoint is already there.

`server` is a default-on feature of the CLI. `zsql serve` fills
`Config` from flags: bind, data dir defaulting to `$ZSQL_HOME/data`,
accounts off by default. Inside a project it serves that project: it
deploys the directory once the server is up and redeploys as the files
change, unless `--no-project` says otherwise; outside a project it
serves what its data directory holds. A non-loopback bind with
accounts off is refused unless `--open` is given, with a message saying
why. `zsql server start|stop|status|logs` re-executes `zsql serve`
detached with a pidfile and log under `ZSQL_HOME`; `stop` sends
SIGTERM; `status` reads the pidfile and calls `/healthz`. The server
takes a `flock` on `<data_dir>/lock` so two instances cannot share a
directory, and the refusal names the other pid. SIGTERM and ctrl-c
trigger axum's graceful shutdown. A deploy writes the branch beside
the live one and swaps it in by rename, so a kill mid-deploy leaves the
previous branch whole; what a crash leaves behind is swept at startup.
The library reads no environment, enforced by a test; the CLI's serve
module is the one place a `ZSQL_*` variable is read. `server install` (a
systemd user unit or a launchd agent) is parked until someone asks.

Embedding beats downloading: one artefact, one version, one thing to
test. The engine is already in the CLI for local planning; the HTTP and
accounts layer on top of it is the smaller addition.

### Build and CI

`warehouse` and `server` give four feature combinations; CI checks
them, runs fmt, clippy with warnings denied, the tests, the goldens and
`cargo deny`, on every push and pull request. This lands before the repo goes public. The README's
build section says the default CLI build needs a C compiler for SQLite
and that `--no-default-features` is the slim build. musl's allocator is
noted in the release notes; a `mimalloc` feature waits for someone to
measure a need.

### Fixtures and publishing

`fixtures/projects/tpcds` and `fixtures/projects/grocery-store`, copied
from local projects with `datasources.yml` reduced to name, adapter and
tier. Customer-service follows later. The golden universes get one read
of their table and field names before the flip to public. The devtool's
private shorthand parser is diffed against `zsql-shorthand` over the
goldens' shorthand lines before it is deleted; any form only the devtool
accepts is ported or consciously dropped.

Golden cases and universes under `fixtures/golden` are never edited as
part of this work. They are the oracle the refactor is checked against.

## Phases

Each is one PR that leaves the tree releasable.

| # | contents | done when |
|---|---|---|
| 1 | `zsql-service` extracted; server refactored onto it; `Trust` enum; `Send + Sync` assertion; determinism test planning one spec on two threads and in two processes | isolation tests, handler tests and goldens pass unchanged; the thread-local counter is proven not to reach any alias, or fixed in the planner if it does |
| 2 | `-r` and the implication rules; local reads; `show.rs` split; lazy git; per-command SIGPIPE; `ZSQL_HOME`; `--json` error contract; TPC-DS and grocery-store fixtures; parity test against an in-process server via `tower::oneshot` | byte-identical SQL and identical error JSON, local versus server, over the fixtures |
| 3 | Local repl with the held model, `Loaded.sources`, mtime reload, `.reload` | reload after an edit, and a broken edit keeps the old model |
| 4 | measure the loader step by step, fix what is slow, bench numbers in the PR; cache only if budgets missed | TPC-DS under 25 ms for `sql`, under 15 ms for reload |
| 5 | CI workflow with the feature matrix, fmt, clippy, tests; README and docs rewritten for the local rule; devtool loses `plan`, `repl`, `test`, `deploy`, `remote`, `values` and its parser after the diff; `publish = false` everywhere; release 0.6.0 | CI green on the matrix; `cli-v0.6.0` tagged |
| A | Server library split with `Config`, env reads out of the library, `OnceLock` removed; `zsql serve` serving the project at hand; data-dir lock; graceful shutdown; atomic deploy writes; bind refusal; `server start/stop/status/logs`; still 0.6.0 | an embedded server survives a client closing early, a SIGTERM, and a second instance on its data dir; then `cli-v0.6.0` |
| B (later) | release workflows merged into one public release carrying the CLI (which is the server); `server install` if asked for; any deploy rework | one tag publishes everything |

Done on 2026-10-15, ahead of the repo going public: `docs/running-it.md`,
`docs/mcp.md` and `ops/` no longer name the host, its private address or
anyone's mailbox; the tree and the git history were scanned for secret
values and hold none (the history still names the host in old versions
of the runbook, left as is). Also done: the MIT `LICENSE`, a format
pass under `rustfmt.toml` (140 columns, the "Max" heuristic, so the
dense style survives) checked in CI, `cargo deny` for advisories and
licences, and Dependabot for crates and actions.

## Decisions register

- Local is the default; `-r` is the exception; `check` and `test` follow the rule.
- The shared crate is `zsql-service`; "engine" means the planner.
- Shape limits are a server policy; a local caller may plan any size of spec.
- Confinement is a type, never a default argument.
- The server's configuration is one struct; the library never reads the environment.
- SIGPIPE is decided per command.
- Errors are one shape in both modes, text or JSON.
- The cache is deferred behind measured budgets.
- Embedding beats downloading: one artefact, one version, one thing to test.
- Accounts are off by default locally, and an open bind with accounts off is refused.
- Deploy writes are atomic by rename; shutdown is graceful; data directories are locked.
- Tests never touch the real home directory.
- Crates are not published to crates.io yet.
- TPC-DS and grocery-store are public fixtures now; customer-service later.
- Golden fixtures are read, never edited, without an explicit decision.

## Progress

- 2026-10-15, review: the server library is one module per concern (`auth`, `tenancy`, `error`, `deploy`, `reads`, `accounts_api`, `ui`, `run`, with `mcp`, `limits` and `mail` as before), every item crate-private unless another module needs it, the router naming handlers by module; the isolation and parity tests drive the same router through explicit imports. A deploy's work directory is named by a counter rather than the clock, which is the likely cause of the two isolation tests that failed now and then under a parallel run: two deploys in one process at the same nanosecond shared a directory and the second was refused for files that existed. The no-environment test now scans every server source file.

- 2026-10-15, phase B continued: the warehouse crate and the sampling feature are gone (the CLI never runs SQL; `samples/values.jsonl.gz` is written by the project's own tooling and read as before). The review's findings taken: a reference unit in `ops/zsqld.service` and a first-install step in the runbook; tests strip every `ZSQL_*` variable and prove a flag wins over its variable; `--no-accounts`; the email-failure limit parsed strictly; the auth cache TTL on `Config`; the tag check as its own job ahead of the builds; clap's `env` for the string settings; stale `zsqld` prose cleared. Signal handlers are installed before the listening line, so a stop that arrives at once is graceful.

- 2026-10-15, phase B: `zsqld` is gone; `zsql serve` is the server, with `ZSQL_*` variables as the base and flags over them, read in one place (the CLI's serve module; the library still reads none). One release workflow on a `v*` tag builds `zsql` for four platforms, proves the server starts from the binary, and publishes here and on the installer's repository. The runbook describes the host's unit running `zsql serve`.

- 2026-10-15, phase A: the server crate is a library with one `Config` and `run`; `zsqld` is a thin binary and the one place a `ZSQL_*` variable is read, which a test enforces; the MCP settings live on the app; the data directory is locked with the holder's pid; shutdown is graceful; a deploy is staged beside the branch and swapped in; `zsql serve` serves the project at hand and `zsql server start|stop|status|logs` keeps it running; integration tests cover a client hanging up, a second instance, SIGTERM, the in-project deploy and the background form. Review taken: only a deploy's own `.<branch>.staging-N`/`.previous-N` leftovers are swept (never another dot directory under a mistyped data dir); `start` reaps a child that exits at once and reports the log's tail; whether a server is running is answered by its data directory's lock, not by a pidfile and a pid that may have been reused; `stop` escalates to SIGKILL after 15 s; `status` probes the bind `start` recorded; an explicit `--project` that is missing is an error; one watch loop serves `deploy --watch` and `serve`, and it sees a deleted file; deploys of one branch are serialised; an unspecified bind is reached over loopback.

- 2026-10-15: phase 1 done (`zsql-service` extracted; goldens, isolation tests and the determinism tests pass). Phase 2 done (`-r` and its rules, every read local by default, one error contract, fixtures `tpcds` and `grocery-store`, parity test byte-identical over both). Phase 5 done (ci.yml: clippy with warnings denied, tests, goldens, the slim CLI check, the warehouse check in its own job; the devtool keeps check, export, golden, bench, load-bench and form, and its private parser's tests moved to the shorthand crate; every crate `publish = false`; two stylistic clippy lints allowed workspace-wide, the rest fixed; version 0.6.0; README and running-it updated). Phase 4 done (loader measured and sped up: TPC-DS load 9.6 to 5.0 ms, `zsql sql` 8 to 9 ms end to end; no cache). Phase 3 done (the repl holds its model, stats `Loaded.sources` before each line, reloads and says so, keeps the old model on a failed load, `.reload`; driven through a pipe by `crates/cli/tests/repl.rs`).

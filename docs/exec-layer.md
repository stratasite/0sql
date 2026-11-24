# The execution layer: transports, typed results, tokens

A plan, started 2026-11-23 on branch `exec-layer`. It prepares the
ground an execution service will stand on, in three phases, and stops
before the service itself: what that service is and does is the next
discussion, and `docs/exec.md` is the earlier survey of it.

Nothing here adds a key to `datasources.yml`. Each warehouse has one
best way in and the code picks it.

Phases are the checklist; tick them off here.

## What each warehouse is reached by

| Datasource | Transport | Result types |
|---|---|---|
| Snowflake | ADBC driver when installed, SQL REST API otherwise | Arrow from the driver; text over REST |
| BigQuery | ADBC driver when installed, REST otherwise | Arrow from the driver; text over REST |
| DuckDB | in this process | Arrow, the engine's own |
| Postgres, Redshift | wire protocol (`tokio-postgres`) | text |
| MySQL | wire protocol (`mysql_async`) | text |
| SQL Server | TDS (`tiberius`) | text |
| SQLite | in this process (`rusqlite`) | text |
| ClickHouse, Databricks, Trino, Athena | their HTTP APIs | text |
| Druid | none | none |

"Text" means every column is `Utf8`: the truth about what that transport
is told today. Giving those transports their types is work this plan
leaves open, listed at the end.

### Why ADBC for two and not the rest

Researched 2026-11-23 against ADBC release 25 (26 October 2026) and the
ADBC Driver Foundry's registry.

- **Snowflake (driver 1.16.0) and BigQuery (1.15.0)** are Apache-2.0,
  open source, past 1.0, and replace the two weakest transports here:
  JSON strings, a page at a time.
- **Redshift (1.9.0) and SQL Server (1.8.0)** are mature and bring
  logins this crate lacks (IAM and serverless; Entra ID). They are also
  closed source, binaries under Columnar's Permissive Binary License,
  with only an issue tracker public. Loading one hands a warehouse
  credential to code nobody here can read. **[OWNER]** to decide; they
  stay native until then.
- **PostgreSQL (1.13.0)** is stable but reads `NUMERIC` as a string and
  is reported slow to connect; the native transport stays.
- **Databricks (0.4.0), ClickHouse (0.1.2), Trino and MySQL (0.8.0),
  Druid (alpha)** are early. Databricks and ClickHouse can return Arrow
  over the HTTP transports already here, which is the better route.
- **Athena** has no driver.

### What an ADBC driver costs

A driver is a shared library, written in Go for these two: 65 MB for
Snowflake, 54 MB for BigQuery. Two consequences.

It is not bundled. `dbc install snowflake` puts it where every ADBC
driver manager looks (`ADBC_DRIVER_PATH`, `~/.config/adbc/drivers`,
`/etc/adbc/drivers`), and it is used when it is there. The search never
takes a relative path, so a project directory cannot supply the library
that is handed the credentials.

A static musl binary cannot load a library, and the Linux releases are
static musl (`docs/releasing.md`). So the released Linux `zsql` always
uses REST for these two, and ADBC is reached from a glibc or macOS
build. That is acceptable for the command line and is the first hard
fact about the execution service: the process that runs queries at
volume cannot be the static binary.

### Text reads stay on REST

The catalog and sampled values are read as text, over REST, whether or
not a driver is installed. A sampled value is a string a consumer
matches against; it must not be spelled one way on a machine with the
driver and another way without.

## Phases

- [x] **Phase 0 — the result contract.** `zsql_datasource::Batches`: a
  schema and Arrow record batches. `Source::batches` beside
  `Source::query`, defaulting to the text read as `Utf8` columns, so
  every adapter answers it. `Source::transport` says `native`, `rest`
  or `adbc`. DuckDB hands over its own batches.
- [ ] **Phase 1 — ADBC for Snowflake and BigQuery.** `crates/datasource/src/adbc.rs`:
  the driver loaded on first use, the REST transport's credentials
  mapped to driver options, the row cap and the statement timeout kept.
  The statement runs on its own thread; at the timeout the caller is
  answered whether or not the driver can cancel, and told which. The
  SQLite driver cannot (found 2026-11-23, when a test waiting on its
  cancellation never returned). Written and
  unit-tested. Run on 2026-11-23 against the real drivers without an
  account: typed batches through the SQLite driver; the Snowflake
  driver took every option and reached its login request for all five
  logins; the BigQuery driver took its options and stopped at the
  missing credentials; all three loaded into one process. Run against
  a BigQuery account the same day: the driver logged in with the key
  file and ran the statement, then could not read the result, because
  its reader uses the Storage Read API and the account lacked
  `bigquery.readsessions.create` (`roles/bigquery.readSessionUser`).
  REST needs no such permission, so a datasource that answered before
  the driver was installed would have stopped answering. `WithAdbc`
  now gives the driver up for the connection on that error, reads over
  REST and says so in its warnings; the first statement runs twice.
  Run against that account on 2026-11-23, with the driver installed
  and the role missing: the statement answered over REST, the
  transport read `rest`, the warning named the permission and the
  role, and a second statement went straight to REST.
  With the role granted, the same day: the driver read through the
  Storage Read API and every type arrived as itself (`Int64`,
  `Decimal128(38, 9)` for NUMERIC, `Decimal256(76, 38)` for BIGNUMERIC,
  `Date32`, `Timestamp(µs, "UTC")`, `Time64`, `List`, `Struct`), where
  REST gives a TIMESTAMP as `1.767323045123456E9` and an array as its
  JSON wrapper. A first statement took 2.5 s and 1,000 rows of a table
  2.3 s; the row cap refused; a statement past a 3 s timeout was
  cancelled by the driver in 3.25 s. One fault found and fixed: a
  timestamp labeled `UTC` could not be printed without a time zone
  database, so it is printed at offset `+00:00`, as `...Z`.
  A NUMERIC prints at its full scale through the driver
  (`12.500000000`) and as written over REST (`12.5`): the two result
  shapes of one datasource, listed below.
  **Still unknown**: everything about a Snowflake account.
- [x] **Phase 2 — results stored as Arrow.** A stored result is
  `rows.arrow`, an Arrow IPC file, LZ4-compressed; manifest format 2
  records the transport and each column's Arrow type. Results written
  before are still read.
- [x] **Phase 3 — an OAuth token as the credential.**
  `Secrets::oauth_token`, taken by Snowflake, BigQuery, Databricks,
  Trino and SQL Server; `takes_oauth_token(adapter)` says which. Whoever
  hands the token over keeps it fresh: nothing here refreshes one, and a
  connection lives no longer than its token.

## Running the driver tests

Three tests in `crates/datasource/src/adbc.rs` load real drivers and are
ignored by default. They need no account: two run statements through
the SQLite driver, and one opens the Snowflake and BigQuery drivers
with fake values to check that every option sent is one the driver
knows.

```
dbc install sqlite snowflake bigquery
cargo test -p zsql-datasource adbc::tests -- --ignored --nocapture
```

Neither `bin/ci` nor the workflow runs them: the Sources step skips
`adbc::tests`, because nothing there installs a driver. Until a step
does, a green run says nothing about the driver path.

## Must do, later

### The command line, set aside on 2026-11-23

The command line's side of this is deferred as a whole; the server
side comes first. What is owed to it:

- **ADBC on the Linux release.** The static musl binary cannot load a
  driver, so `zsql run` there is always REST for Snowflake and
  BigQuery. The intended route is a separate, dynamically linked
  executor the command line hands a query to, not a dynamic `zsql`.
- **Installing a driver without a second tool.** Today a driver arrives
  by `dbc install`, Columnar's tool, which the user has to install
  first. `zsql` should fetch the package itself, check its signature
  and write the manifest. How the signature is verified has not been
  looked at.
- **One datasource, two result shapes.** The same `zsql run` stores
  typed columns on a Mac with the driver and `Utf8` columns on Linux.
  The manifest says which transport ran; nothing reconciles them.
- **The OAuth flow, typed output**: the two entries below that name
  the command line.

### The rest

- **The command line's OAuth flow.** `oauth_token` is read from
  `.zsql` and `ZSQL_DS_<UID>_OAUTH_TOKEN` like any secret, which is
  enough to test with. Obtaining one, a browser or device flow and a
  refresh, is not built.
- **The principal in the cache key.** With a token per end user, two
  users of one datasource see different rows. `Connector::identity`
  must name the principal, or one user's result answers another's
  question. The command line's identity does not include a token today
  (`crates/cli/src/run.rs` says so); a server's connector must, from
  what its upstream tells it.
- **A live run of phase 1 against Snowflake**: PAT, key-pair and OAuth
  logins through the driver; that the timeout cancels a running
  statement; what `NUMBER` and timestamps arrive as. BigQuery's run is
  recorded under phase 1.
- **Types from the text transports.** Postgres, MySQL, SQL Server,
  SQLite, Trino and Athena know their column types and discard them;
  ClickHouse and Databricks can answer in Arrow outright.
- **Typed output.** `zsql run --format json` prints every value as a
  string, as it always has. With types on hand a number could be a
  number; that changes a published format and is its own decision.
- **Streaming.** `Batches` is a whole result under a row cap. A result
  larger than memory needs the batches as a stream, which is the
  execution service's shape and is designed with it.

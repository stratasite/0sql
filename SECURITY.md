# Security

0sql plans SQL from a model and never connects to a warehouse, so the
surface is the server: who may deploy, who may read which branch, and
what a request can make it do. Tenant isolation is the first concern
of the design, and `crates/server/src/isolation_tests.rs` is the
suite that holds it.

## Reporting a vulnerability

Please do not open a public issue for a security problem. Write to
**info@strata.do** with what you found, how to reproduce it, and
the version (`zsql --version`). You will hear back within three
working days, and the fix goes out as a release with the report
credited to you unless you would rather it were not.

## What is in scope

- Reaching another account's deployments, keys or members.
- Deploying, planning or reading a branch a key was not granted.
- A request that makes the server read or write outside its data
  directory, or execute anything.
- Bypassing the rate limits, the admission caps or the request bounds
  in a way that costs the server more than the limits allow.
- Session or key handling in the web app and the MCP endpoint.

## What is not

- The SQL that comes back: 0sql returns a statement and never runs
  it. Whether the statement is safe to run against your warehouse,
  with your credentials, is your application's decision.
- A security context that is wrong for the user. The context is sent
  by the caller, or bound to a query key by the project's owner; the
  server plans for whoever it is told.
- Denial of service against a server run with `ZSQL_LIMITS=off`, or
  one bound beyond the loopback with accounts off and `--open`, both
  of which the operator chose.

## Supported versions

The newest release on the main branch. There are no maintenance
branches; a fix is a new version.

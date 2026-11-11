# Security

0sql plans SQL from a model and never connects to a warehouse, so the
surface is the server, and the server is built to sit behind your own
service the way a database does. It has no accounts and no keys: it
answers whoever can reach it, plans under the security context the
caller sends, and leaves who may ask, and how often, to what is in
front of it. The one credential is the optional deploy token, which
keeps deploys, validates, tests and removals to whoever holds it.
`crates/server/src/tests.rs` is the suite that holds the server to
this; `docs/operating.md` says where the server belongs.

## Reporting a vulnerability

Please do not open a public issue for a security problem. Write to
**info@strata.do** with what you found, how to reproduce it, and
the version (`zsql --version`). You will hear back within three
working days, and the fix goes out as a release with the report
credited to you unless you would rather it were not.

## What is in scope

- Deploying, validating, testing or removing a branch without the
  deploy token, on a server that has one.
- A request that makes the server read or write outside its data
  directory, or execute anything.
- An archive that reads anything outside itself, or leaves something
  behind after a validate.
- A plan that returns rows a policy should have filtered, given the
  context that was sent.
- Bypassing the admission caps or the request bounds in a way that
  costs the server more than they allow.

## What is not

- The SQL that comes back: 0sql returns a statement and never runs
  it. Whether the statement is safe to run against your warehouse,
  with your credentials, is your application's decision.
- A security context that is wrong for the user. The context is sent
  by the caller, in the request body or the `Zsql-Context` header, and
  the server plans for whoever it is told.
- Reading a model's catalog or planning SQL on a server you can reach.
  Every read is open by design; a server reachable by callers it should
  not answer is a deployment to fix.
- Deploying to a server run without a deploy token, or flooding one
  that has no rate-limiting proxy in front. Both are the operator's
  choice.

## Supported versions

The newest release on the main branch. There are no maintenance
branches; a fix is a new version.

# Contributing

Thanks for looking. Issues and pull requests are welcome; the notes
below are what a change needs to get in.

## Building and testing

The toolchain `rust-toolchain.toml` names, and a C compiler for the
SQLite behind accounts.

```sh
cargo build --release -p zsql-cli                         # target/release/zsql
cargo test --workspace
cargo run --release -p zsql-devtool -- golden --strict    # the 385 golden cases
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

CI runs all of that, plus `cargo deny` over the dependency tree and a
macOS compile of the CLI, on every pull request.

## The goldens are the specification

`fixtures/golden` holds 385 cases captured from the planner this one
was written against: a spec, a security context, a universe, and the
SQL or the refusal expected. They are never edited to make a change
pass. A change that alters planned SQL is a change to the contract, and
the pull request should say why the new statement is right and the old
one was not; adding a case is the way to pin new behaviour.

## What a pull request looks like

- One concern per pull request, with a message that says what changed
  and why, written for someone reading `git log` next year.
- Tests for behaviour, next to the code that has it. The server's
  isolation tests drive the real router; a change to who may reach what
  belongs there.
- Formatted with `rustfmt` (the width is in `rustfmt.toml`), clean under
  clippy with warnings denied.
- No new dependency without a sentence on why an existing one would not
  do; the CLI is a single static binary and every crate ships in it.
- Docs in `docs/` are design notes: update the one that describes what
  you changed. User-facing documentation lives at
  [0sql.io/docs](https://0sql.io/docs/) and is maintained separately.

## Reporting a bug

A spec, the model it was planned against (or a reduced one), the SQL or
refusal you got, and what you expected. `zsql explain --expr ...` and
`zsql --version` in the report save a round trip.

## Security

See [SECURITY.md](SECURITY.md). Please do not file security problems as
issues.

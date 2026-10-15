# Golden import

The golden cases under `fixtures/golden` come from strata's planner test
suite, captured by a hook that records every planned query, its universe
snapshot, its spec and the SQL or error strata produced, then replays it
on a mutated clone branch. They are the specification this planner is
held to.

This service plans specs only, so `import_goldens.py` keeps, per case: the
spec, the security context, the snapshot hash and the expected outcome.
Cases the spec cannot express are left out: a view's report-owned segments
with per-target links, linked filters used as segment criteria, static
calculations injected by a test, a segment planned directly. So are the
seven cases where strata itself answers differently per branch.

```
harness/import_goldens.py <capture checkout> lossy-ids.txt
```

`lossy-ids.txt` lists the case ids the capture's scoreboard marks lossy
on the spec path (one id per `*.spec-lossy.*` file in its dump).

## Case

| key | meaning |
|---|---|
| `id` | `TestClass#test_name#n`, with `@dev` for the replay on the mutated branch |
| `variant` | `main` or `dev` |
| `universe` | hash of the snapshot in `universes/` |
| `context` | security context, or null |
| `spec` | the query spec |
| `expected` | `{sql}` or `{error: {class, message}}` |
| `mutations` | dev only: what the clone changed |

# The MCP instruction-set eval

An agent connected to the server over MCP answers questions about the customer
service demo model by writing query specs. This harness measures how well,
and it is what the instruction set in `crates/server/src/mcp_prompts.json`
and the spec schema in `crates/server/src/mcp_spec_schema.json` are tuned
against. Accuracy first, then turns and seconds.

## What one run is

For every case in `cases.jsonl`, Claude Code is started headless as a real
MCP client (`claude -p`, the zsql server as its only tool source, every
built-in tool denied, an empty working directory) with the question wrapped
in `task.md`. The agent finishes by printing the spec it settled on; the
runner plans that spec through the server's REST route with the same key,
runs the SQL on the demo warehouse, and compares the rows with the rows the
case's reference spec produces. Same rows, in order when the case says
`ordered`, is a pass. Column names are ignored, column order is ignored,
numbers are compared to six significant digits.

That makes the grade about the answer, not the path: a different but
equivalent spec passes, a plausible-looking wrong one fails, and nothing a
model can read reaches the references, which live only in this directory
and are never sent to it.

Per case the row records the grade, the turns and tool calls, tool errors,
latency and cost as Claude Code reports them, the token usage, and the spec
the agent chose; the train split's full transcripts go to `traces/`.

## Running it

Needs the local server up with the demo model deployed and a query key for
it bound to `{"system_admin": true}` in `.key` (gitignored), the demo repo at
`~/Projects/demo/0sql-dashboard-demo` with its DuckDB warehouse built, and
`claude` on the path with a login.

```
python3 run.py --check-refs                 # every reference plans, runs, and grades itself a pass
python3 run.py --null-check                 # a wrong spec per case fails
python3 run.py --variant baseline --reps 2  # a full pass; resumable
python3 run.py --variant v1 --description "what changed"
```

Results land under `hillclimb/<variant>/` in the layout the hillclimb
report builder reads (`results.jsonl`, `errors.jsonl`, `traces/`,
`summary.json`); `hillclimb/_state.json` holds the train/test split, drawn
once at random stratified by the case's first tag. The headline is the test
split's pass rate; the analyzer that proposes changes reads train traces
only.

## Cases

Sixty-one questions over the demo model, tagged by the capability they
exercise: `basic` projections and published rates, `join` for role-playing
and second-hop dimensions, `filter` for every predicate shape including
relative dates and an and/or tree, `rank` for top-n, ordering and limit,
`decorator` for truncate, extract, window, contribute and temporalize,
`calc` for formulas, `segment` for include, exclude, apply_to and
measure-defined cohorts, `cross` for measures from other facts, and `shape`
for aliases and ordering. Every reference was checked against the
warehouse by hand when written, and `--check-refs` keeps them honest.

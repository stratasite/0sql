| round | variant | test | train | s/case | calls | turns | tool err | $/case | spend |
|---|---|---|---|---|---|---|---|---|---|
| 0 | baseline | 0.95 | 1.00 | 17.4 | 5.39 | 9.54 | 0.11 | $0.39 | $48 |
| 1 | v1 | 0.90 | 0.97 | 13.7 | 3.38 | 6.98 | 0.16 | $0.31 | $66 |
| 2 | v2 | 0.86 | 1.00 | 13.6 | 3.30 | 6.80 | 0.14 | $0.31 | $104 |
| 3 | v3 | 0.88 | 1.00 | 13.6 | 3.29 | 6.75 | 0.09 | $0.31 | $142 |
| 4 | v4 | 0.86 | 1.00 | 14.0 | 3.40 | 7.10 | 0.15 | $0.32 | $180 |
| 5 | v5 | 0.95 | 1.00 | 15.7 | 4.53 | 8.92 | 0.16 | $0.39 | $228 |

## Recommended change

v5, now committed in `crates/server/src/mcp_prompts.json` and `mcp_spec_schema.json`: skip `list_projects` when the project is known; `fields` is a search, one call per phrase of the request; same-worded fields on other tables are different things and the name's qualifier and the tables decide; the chosen uid goes in the spec; and the schema states the window decorator's real rules (mode required, size is rows preceding, order_by and partition_refs take projected uids), points period-over-period at temporalize, and prefers uids in field references.

## Versus baseline

On claude-opus-5, two reps each: test 0.95 to 0.95 and train 1.00 to 1.00, so no accuracy change on the stronger client; tool calls 5.4 to 4.5 per case, 17.4 s to 15.7 s, cost unchanged per case, and no spec retries left on train (baseline had cases spending three or four `sql` calls learning the window decorator). On claude-sonnet-5, one rep: train 0.95 to 1.00 and test 0.90 to 0.95, calls 5.4 to 5.0; the three cases it gained were the two temporalize cases, where it had used a lag window, and a top-n ordering.

## Why trust this

Every case is deterministic across reps on Opus, so a flip is a change, not noise; the two reference errors and three ambiguous prompts found at baseline were fixed and re-scored under each variant's own instruction files before any comparison; the analyzer that proposed changes read train traces only and the headline is the test split; the grader passes every reference and fails every deliberately wrong spec; and the winner was confirmed on a second model.

## What else was tried

v1 (one broad `fields` call for everything) and v3 (v1 plus the same-worded-field rule) run at 3.3 calls and 13.6 s, 39% fewer calls and 22% less time than baseline, but lose one test case every rep: a question qualifying an attribute by its role, where a broad search ranks the plain field first and can leave the qualified one out of the ranked band. v4 added an extra search for qualified phrases and did not recover it. v2 alone (the schema fixes) kept train at 1.00 and the efficiency but not the test case. The remaining miss on both models is a segment keyed on an id that two fact tables carry; it resisted three rounds and is a candidate for a model-side fix (a description on the field) rather than more instruction text.

## Before and after, on train cases

| case | baseline | v5 |
|---|---|---|
| deco_lag | 27 s, 5 calls: list_projects, fields, sql, sql, sql (two refused specs while learning `mode` and `order_by`) | 14 s, 2 calls: fields, sql |
| basic_by_site | 13 s, 4 calls: list_projects, fields "call center", fields "contacts", sql | 10 s, 3 calls: the same without list_projects |
| calc_kb_per_contact | 42 s, 9 calls: list_projects, five fields searches, explain, sql | 28 s, 6 calls: four fields searches, sql, sql |
| join_customer_region | 16 s, 4 calls | 13 s, 3 calls |

## Failure taxonomy

No harness or serving errors in any scored round. Every failure is a well-formed spec the planner accepted that answers a different question: a same-worded field from another table (the persistent one), a lag window where temporalize was meant (Sonnet, fixed by v2's schema text), an extra sort column the question did not ask for (Opus, two rounds, flaky), and a BPO question whose wording invited excluding in-house sites (fixed as a prompt).

## What to try next

- The persistent miss is an id both the contact fact and an A/B allocation table carry. A description on those fields in the model would resolve it at the source; the `fields` result would then carry the distinction the instructions ask the client to read.
- The extra-column flakiness is instruction-following on the eval's own task prompt, not on the server's instructions; it is not worth more server text.
- The efficient variant (v3) is one sentence away from v5. If a future model or a richer `fields` result ranks qualified fields reliably, v3's single search is a 25% cut in calls waiting to be taken.
- Claude Code costs one ToolSearch call and a large system prompt per run; the demo app's own agent at two turns and five seconds is not reachable through Claude Code regardless of instructions. The `instructions` and tool descriptions also serve claude.ai connectors and API MCP clients, which this eval did not run.

## Spend

About $228 on Opus 5 across the baseline and five rounds (two reps on baseline, v2 to v5; one on v1), plus $21 on the two Sonnet 5 passes.

| round | variant | test | train | s/case | calls | round trips | tool err | $/case | spend |
|---|---|---|---|---|---|---|---|---|---|
| 0 | baseline | 0.95 | 1.00 | 17.4 | 5.39 | 6.39 | 0.11 | $0.39 | $48 |
| 1 | v1 | 0.90 | 0.97 | 13.7 | 3.38 | 4.38 | 0.16 | $0.31 | $66 |
| 2 | v2 | 0.86 | 1.00 | 13.6 | 3.30 | 4.30 | 0.14 | $0.31 | $104 |
| 3 | v3 | 0.88 | 1.00 | 13.6 | 3.29 | 4.29 | 0.09 | $0.31 | $142 |
| 4 | v4 | 0.86 | 1.00 | 14.0 | 3.40 | 4.40 | 0.15 | $0.32 | $180 |
| 5 | v5 | 0.95 | 1.00 | 15.7 | 4.53 | 5.53 | 0.16 | $0.39 | $228 |
| 6 | v6 | 0.93 | 1.00 | 15.8 | 4.16 | 4.65 | 0.11 | $0.34 | $270 |
| 7 | v7 | 0.90 | 0.97 | 14.8 | 3.97 | 4.75 | 0.15 | $0.27 | $303 |
| 8 | v8 | 0.98 | 1.00 | 11.6 | 3.22 | 3.36 | 0.07 | $0.28 | $337 |
| 9 | v9 | 0.95 | 1.00 | 11.5 | 3.25 | 3.35 | 0.07 | $0.28 | $371 |
| 10 | v9-default | 0.95 | 1.00 | 14.9 | 3.98 | 4.75 | 0.07 | $0.33 | $411 |
| 12 | v11 | 1.00 | 1.00 | 7.5 | 1.07 | 2.03 | 0.05 | $0.35 | $433 |
| 13 | v12 | 0.95 | 1.00 | 11.1 | 3.27 | 3.31 | 0.10 | $0.31 | $471 |
| 14 | v13 | 0.90 | 0.97 | 12.5 | 2.30 | 3.29 | 1.13 | $0.21 | $496 |

## Recommended change

The committed server (v12 content): spec-only tools with the schema in the tool definitions; instructions under 2,000 characters; the catalogue of every branch the key may read appended to `initialize` and returned whole by `fields` with no q; dimension descriptions in the catalogue; a field refusal carrying the nearest fields with uid, kind, description and table; `ZSQL_MCP_TOOLS` to expose a subset. For Claude Code users, `ENABLE_TOOL_SEARCH=false`.

## Versus baseline, on claude-opus-5

Client as installed: test 0.95 to 0.95, train 1.00 to 1.00, 17.4 s to 14.9 s, 6.4 to 4.8 round trips per question. Client with tool search off: 11.5 s and 3.4 round trips, test 0.95 to 0.98 depending on one flaky case. With the catalogue visible to the model (v11), which this client does not allow but which is what the demo application's prompt gives its model: 7.5 s, 2.0 round trips, 1.00 on both splits.

## Why the demo answers in two turns and MCP through Claude Code does not

Measured, not inferred. (1) Claude Code keeps about 2,000 characters of a server's `initialize` instructions and of a tool description, marking the rest `[truncated]`; two headless probes quoted the cut. The demo's catalogue lives in its own system prompt, so its first turn is the query. (2) Claude Code defers MCP tool definitions behind a `ToolSearch` round trip; `ENABLE_TOOL_SEARCH=false` removes it and lets the model run its searches in parallel. (3) Claude Code sends 40,000 to 96,000 tokens of context per model call, nearly all cached, against the demo's 10,000; a round trip costs 2.7 to 3.4 s on Opus 5. (4) Given a `fields` tool and no catalogue, the model searches before it writes, instruction or not (v12), and without a `fields` tool it writes, is refused, and retries (v13), so three round trips is the floor on this client. Nothing in MCP itself is slower than function calling: v11 is the same server and client with the catalogue in view, and it behaves exactly like the demo.

## What else was tried

v1 to v5: the instruction text (REPORT.md's first section). v6: catalogue in `initialize`, no effect, which led to the cap probe. v7: `fields` with no q as the catalogue. v8: tool search off. v9: dimension descriptions in the catalogue. v10: catalogue in the `sql` tool description, truncated the same way, not run. v12: sql first with refusals carrying candidates, no behaviour change. v13: only `sql` exposed, same round trips, lower accuracy. One round was voided by the subscription's session limit and re-run.

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

About $496 on Opus 5 across the baseline and thirteen rounds, plus $21 on the two Sonnet 5 passes.

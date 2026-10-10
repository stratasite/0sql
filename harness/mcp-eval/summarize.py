#!/usr/bin/env python3
"""Status table over every variant in a flow: test/train pass, s/case, calls, turns, errors, $/case, spend."""
import json, sys, statistics as st, os
from pathlib import Path
flow = Path(sys.argv[1] if len(sys.argv) > 1 else "hillclimb")
state = json.load(open(flow / "_state.json")); train, test = set(state["train_ids"]), set(state["test_ids"])
variants = ["baseline"] + sorted([d.name for d in flow.iterdir() if d.is_dir() and d.name.startswith("v")], key=lambda v: int(v[1:]))
spend = 0
print("| round | variant | test | train | s/case | calls | turns | tool err | $/case | spend |")
print("|---|---|---|---|---|---|---|---|---|---|")
for i, v in enumerate(variants):
    p = flow / v / "results.jsonl"
    if not p.exists(): continue
    rows = [json.loads(l) for l in open(p) if l.strip()]
    def pr(ids):
        rs = [r for r in rows if r["prompt_id"] in ids]; return sum(r["grade"]["pass"] for r in rs) / len(rs) if rs else float("nan")
    cost = sum(r["cost_usd"] or 0 for r in rows); spend += cost
    errs = len(open(flow / v / "errors.jsonl").read().splitlines()) if (flow / v / "errors.jsonl").exists() else 0
    print(f"| {i} | {v} | {pr(test):.2f} | {pr(train):.2f} | {st.mean(r['latency_s'] for r in rows):.1f} | {st.mean(r['tool_calls'] for r in rows):.2f} | {st.mean(r['turns'] for r in rows):.2f} | {st.mean(r['tool_errors'] for r in rows):.2f} | ${cost/len(rows):.2f} | ${spend:.0f} |" + (f"  ({errs} harness errors)" if errs else ""))
if len(sys.argv) > 2:
    v = sys.argv[2]
    rows = [json.loads(l) for l in open(flow / v / "results.jsonl") if l.strip()]
    print(f"\n{v} failures:")
    for r in rows:
        if not r["grade"]["pass"]:
            print(f"  {'TEST ' if r['prompt_id'] in test else 'train'} {r['tags'][0]:9s} {r['prompt_id']:28s} {r['explanation']['pass'][:110]}")
    from collections import Counter
    seqs = Counter()
    for f in os.listdir(flow / v / "traces"):
        t = json.load(open(flow / v / "traces" / f)); seqs[" > ".join(x["name"] for x in t if x["role"] == "tool_call" and x["name"] != "ToolSearch")] += 1
    print(f"\n{v} train tool sequences:")
    for s, n in seqs.most_common(8): print(f"  {n:3d}  {s}")

#!/usr/bin/env python3
"""The MCP instruction-set eval: Claude Code, as a real MCP client, answers
questions about the demo model through the local zsqld, and the spec it
settles on is graded by running its SQL against the demo warehouse next to a
reference spec's SQL. See README.md.

    run.py --variant baseline --reps 1 --model claude-opus-5
    run.py --check-refs          # every reference plans, runs and passes its own grade
    run.py --null-check          # a wrong spec fails the grade
"""
import argparse, json, os, random, re, shutil, subprocess, sys, tempfile, threading, time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import urllib.request

HERE = Path(__file__).resolve().parent
SERVER = os.environ.get("ZSQL_EVAL_SERVER", "http://127.0.0.1:3699")
PROJECT = os.environ.get("ZSQL_EVAL_PROJECT", "customer-service")
BRANCH = os.environ.get("ZSQL_EVAL_BRANCH", "main")
KEY = os.environ.get("ZSQL_EVAL_KEY") or (HERE / ".key").read_text().strip() if (HERE / ".key").exists() or os.environ.get("ZSQL_EVAL_KEY") else None
MCP_TOOLS = ["list_projects", "fields", "tables", "values", "explore", "sql", "explain"]
DENIED_TOOLS = "Read,Write,Edit,MultiEdit,Bash,Glob,Grep,WebFetch,WebSearch,Agent,Task,NotebookEdit,TodoWrite,LS"

# ----------------------------------------------------------------- planning

def plan(spec):
    req = urllib.request.Request(f"{SERVER}/projects/{PROJECT}/branches/{BRANCH}/sql", data=json.dumps({"spec": spec}).encode(),
                                 headers={"authorization": f"Bearer {KEY}", "content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        try:
            return json.load(e)
        except Exception:
            return {"error": {"class": "Http", "message": f"status {e.code}"}}

def run_sql(sqls):
    out = subprocess.run(["node", str(HERE / "duck.mjs")], input=json.dumps(sqls), capture_output=True, text=True, timeout=120)
    if out.returncode != 0:
        raise RuntimeError(out.stderr[-2000:])
    return json.loads(out.stdout)

# ------------------------------------------------------------------ grading

def norm(v):
    if v is None:
        return "∅"
    if isinstance(v, bool):
        return str(v)
    if isinstance(v, (int, float)):
        return f"{float(v):.6g}"
    s = str(v)
    try:
        f = float(s)
        if re.fullmatch(r"-?\d+(\.\d+)?([eE][-+]?\d+)?", s):
            return f"{f:.6g}"
    except ValueError:
        pass
    if s.endswith(" 00:00:00"):
        s = s[:-9]
    return s

def columns_of(result):
    cols = result["columns"]
    return [[norm(row.get(c)) for row in result["rows"]] for c in cols]

def align(ref_cols, cand_cols):
    """A permutation of the candidate's columns that matches the reference's,
    by equal multisets of values; None when there is none."""
    if len(ref_cols) != len(cand_cols):
        return None
    sig = lambda col: tuple(sorted(col))
    cand_sigs = [sig(c) for c in cand_cols]
    used, perm = set(), []
    for rc in ref_cols:
        s = sig(rc)
        j = next((k for k, cs in enumerate(cand_sigs) if cs == s and k not in used), None)
        if j is None:
            return None
        used.add(j)
        perm.append(j)
    return perm

def same_result(ref, cand, ordered):
    """Pass, reason."""
    if "error" in ref:
        return False, "reference sql failed: " + ref["error"][:200]
    if "error" in cand:
        return False, "candidate sql failed: " + cand["error"][:200]
    if len(ref["rows"]) != len(cand["rows"]):
        return False, f"{len(cand['rows'])} rows, expected {len(ref['rows'])}"
    rc, cc = columns_of(ref), columns_of(cand)
    perm = align(rc, cc)
    if perm is None:
        return False, f"columns differ: candidate {cand['columns']} vs expected {ref['columns']}"
    cc = [cc[j] for j in perm]
    ref_rows = list(zip(*rc)) if rc else []
    cand_rows = list(zip(*cc)) if cc else []
    if ordered:
        if ref_rows != cand_rows:
            return False, "same rows, different order"
    elif sorted(ref_rows) != sorted(cand_rows):
        return False, "rows differ"
    return True, "match"

_ref_cache = {}
_ref_lock = threading.Lock()

def reference(case):
    with _ref_lock:
        if case["id"] in _ref_cache:
            return _ref_cache[case["id"]]
    planned = plan(case["spec"])
    if "error" in planned:
        res = {"error": "plan: " + json.dumps(planned["error"])}
    else:
        res = run_sql([planned["sql"]])[0]
        res["sql"] = planned["sql"]
    with _ref_lock:
        _ref_cache[case["id"]] = res
    return res

def grade(case, spec):
    """Grade dict, reason, candidate sql."""
    if spec is None:
        return {"pass": 0, "plans": 0}, "no spec", None
    planned = plan(spec)
    if "error" in planned:
        return {"pass": 0, "plans": 0}, "plan error: " + json.dumps(planned["error"])[:300], None
    ref = reference(case)
    cand = run_sql([planned["sql"]])[0]
    ok, reason = same_result(ref, cand, case.get("ordered", False))
    return {"pass": 1 if ok else 0, "plans": 1}, reason, planned["sql"]

# ------------------------------------------------------------ the agent run

FENCE = re.compile(r"```(?:json)?\s*(\{.*?\})\s*```", re.S)

def final_spec(text):
    blocks = FENCE.findall(text or "")
    for b in reversed(blocks):
        try:
            v = json.loads(b)
            if isinstance(v, dict):
                return v.get("spec", v) if "projections" not in v and "spec" in v else v
        except json.JSONDecodeError:
            continue
    t = (text or "").strip()
    if t.startswith("{"):
        try:
            v = json.loads(t)
            return v.get("spec", v) if "projections" not in v and "spec" in v else v
        except json.JSONDecodeError:
            pass
    return None

def run_agent(question, model, mcp_config, max_turns, timeout_s):
    prompt = (HERE / "task.md").read_text().replace("{question}", question)
    work = tempfile.mkdtemp(prefix="mcp-eval-")
    cmd = ["claude", "-p", prompt, "--mcp-config", mcp_config, "--strict-mcp-config",
           "--allowedTools", "mcp__zsql__*", "--disallowedTools", DENIED_TOOLS,
           "--output-format", "stream-json", "--verbose", "--model", model, "--max-turns", str(max_turns),
           "--no-session-persistence"]
    t0 = time.time()
    try:
        proc = subprocess.run(cmd, cwd=work, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=timeout_s)
    except subprocess.TimeoutExpired as e:
        shutil.rmtree(work, ignore_errors=True)
        return {"error": "timeout", "stdout": (e.stdout or "")[-4000:] if isinstance(e.stdout, str) else ""}
    wall = time.time() - t0
    shutil.rmtree(work, ignore_errors=True)
    events = []
    for line in proc.stdout.splitlines():
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    if proc.returncode != 0 and not any(e.get("type") == "result" for e in events):
        return {"error": f"claude exited {proc.returncode}: {proc.stderr[-1500:]}"}
    trace, calls, sql_calls, tool_errors, turns, model_seen = [{"role": "user", "content": prompt}], 0, 0, 0, 0, None
    last_text, last_ok_sql_spec, usage = "", None, {"input_tokens": 0, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
    pending, result = {}, {}
    for e in events:
        t = e.get("type")
        if t == "assistant":
            turns += 1
            msg = e["message"]
            model_seen = msg.get("model") or model_seen
            for k in usage:
                usage[k] += (msg.get("usage") or {}).get(k, 0) or 0
            for b in msg.get("content", []):
                if b["type"] == "text":
                    trace.append({"role": "assistant", "content": b["text"]})
                    last_text = b["text"]
                elif b["type"] == "thinking":
                    if b.get("thinking"):
                        trace.append({"role": "assistant", "content": "", "thinking": b["thinking"]})
                elif b["type"] == "tool_use":
                    calls += 1
                    name = b["name"].replace("mcp__zsql__", "")
                    if name == "sql":
                        sql_calls += 1
                    pending[b["id"]] = (name, b["input"])
                    trace.append({"role": "tool_call", "name": name, "content": json.dumps(b["input"])})
        elif t == "user":
            content = e.get("message", {}).get("content")
            if isinstance(content, list):
                for b in content:
                    if b.get("type") == "tool_result":
                        c = b.get("content")
                        text = c if isinstance(c, str) else json.dumps(c)
                        is_err = bool(b.get("is_error")) or '"isError":true' in text.replace(" ", "")
                        name, inp = pending.get(b.get("tool_use_id"), ("?", None))
                        if is_err:
                            tool_errors += 1
                        elif name == "sql" and isinstance(inp, dict) and isinstance(inp.get("spec"), dict):
                            last_ok_sql_spec = inp["spec"]
                        trace.append({"role": "tool_result", "name": name, "content": text[:6000] + (" …[truncated]" if len(text) > 6000 else "")})
        elif t == "result":
            result = e
    spec = final_spec(last_text)
    source = "final" if spec is not None else ("tool" if last_ok_sql_spec is not None else "none")
    if spec is None:
        spec = last_ok_sql_spec
    return {"trace": trace, "spec": spec, "spec_source": source, "turns": turns, "tool_calls": calls, "sql_calls": sql_calls,
            "tool_errors": tool_errors, "wall_s": wall, "latency_s": (result.get("duration_ms") or wall * 1000) / 1000,
            "api_s": (result.get("duration_api_ms") or 0) / 1000, "cost_usd": result.get("total_cost_usd"),
            "model": model_seen or model, "usage": usage, "num_turns": result.get("num_turns"),
            "subtype": result.get("subtype"), "is_error": result.get("is_error", False), "result_text": result.get("result", "")}

# ------------------------------------------------------------------- the loop

def load_cases(only=None):
    cases = [json.loads(l) for l in (HERE / "cases.jsonl").read_text().splitlines() if l.strip()]
    if only:
        wanted = set(only.split(","))
        cases = [c for c in cases if c["id"] in wanted]
    return cases

def write_mcp_config(flow):
    cfg = flow / ".mcp.json"
    cfg.write_text(json.dumps({"mcpServers": {"zsql": {"type": "http", "url": f"{SERVER}/mcp", "headers": {"Authorization": f"Bearer {KEY}"}}}}))
    return str(cfg)

def ensure_state(flow, cases, seed=7, test_share=0.33):
    state_path = flow / "_state.json"
    if state_path.exists():
        return json.loads(state_path.read_text())
    rnd = random.Random(seed)
    by_tag = {}
    for c in cases:
        by_tag.setdefault(c["tags"][0], []).append(c["id"])
    train, test = [], []
    for tag, ids in sorted(by_tag.items()):
        ids = sorted(ids)
        rnd.shuffle(ids)
        k = max(1, round(len(ids) * test_share))
        test += ids[:k]
        train += ids[k:]
    state = {"current_round": 0, "reps": 1,
             "goal": {"target": "pass", "direction": "higher", "hold": ["latency_s", "cost_usd"]},
             "approve_each_round": False, "best": {"round": 0, "test_score": None},
             "train_ids": sorted(train), "test_ids": sorted(test),
             "harness_paths": ["harness/mcp-eval/run.py", "harness/mcp-eval/duck.mjs", "harness/mcp-eval/task.md", "harness/mcp-eval/cases.jsonl"],
             "metrics": [{"id": "pass", "kind": "binary", "label": "Pass"}, {"id": "plans", "kind": "binary", "label": "Plans"}],
             "perf_fields": [{"id": "latency_s", "label": "Latency", "unit": "s"}, {"id": "cost_usd", "label": "Cost", "unit": "$"},
                             {"id": "tool_calls", "label": "Tool calls"}, {"id": "turns", "label": "Turns"}, {"id": "tool_errors", "label": "Tool errors"}]}
    flow.mkdir(parents=True, exist_ok=True)
    state_path.write_text(json.dumps(state, indent=2))
    return state

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--flow", default=str(HERE / "hillclimb"))
    ap.add_argument("--variant", default="baseline")
    ap.add_argument("--description", default="")
    ap.add_argument("--model", default="claude-opus-5")
    ap.add_argument("--reps", type=int, default=1)
    ap.add_argument("--concurrency", type=int, default=4)
    ap.add_argument("--max-turns", type=int, default=15)
    ap.add_argument("--timeout-s", type=int, default=420)
    ap.add_argument("--cases", default=None, help="comma-separated ids")
    ap.add_argument("--check-refs", action="store_true")
    ap.add_argument("--null-check", action="store_true")
    ap.add_argument("--regrade", action="store_true", help="re-grade stored specs in the variant without running the agent")
    a = ap.parse_args()
    if not KEY:
        sys.exit("no key: put the eval query key in harness/mcp-eval/.key or ZSQL_EVAL_KEY")
    cases = load_cases(a.cases)
    flow = Path(a.flow)

    if a.check_refs or a.null_check:
        bad = 0
        for c in cases:
            ref = reference(c)
            if "error" in ref:
                print("REF ERROR", c["id"], ref["error"]); bad += 1; continue
            if not ref["rows"]:
                print("REF EMPTY", c["id"]); bad += 1; continue
            if a.check_refs:
                g, reason, _ = grade(c, c["spec"])
                if not g["pass"]:
                    print("SELF-GRADE FAIL", c["id"], reason); bad += 1
            if a.null_check:
                wrong = json.loads(json.dumps(c["spec"]))
                # Drop the last projection, or swap the measure for row count: a plausible wrong answer.
                if len(wrong["projections"]) > 1:
                    wrong["projections"] = wrong["projections"][:-1]
                else:
                    wrong["projections"] = [{"field": "Agents"}]
                g, reason, _ = grade(c, wrong)
                if g["pass"]:
                    print("NULL PASSES", c["id"], reason); bad += 1
        print(f"{len(cases)} cases checked, {bad} problems")
        sys.exit(1 if bad else 0)

    state = ensure_state(flow, load_cases())
    vdir = flow / a.variant
    (vdir / "traces").mkdir(parents=True, exist_ok=True)
    results_path, errors_path = vdir / "results.jsonl", vdir / "errors.jsonl"
    done = set()
    if results_path.exists():
        for l in results_path.read_text().splitlines():
            if l.strip():
                r = json.loads(l)
                done.add((r["prompt_id"], r["rep"]))
    (vdir / "summary.json").write_text(json.dumps({"description": a.description or a.variant, "target": "tools", "model": a.model}, indent=2))
    mcp_config = write_mcp_config(flow)
    train = set(state["train_ids"])
    jobs = [(c, k) for c in cases for k in range(a.reps) if (c["id"], k) not in done]
    lock = threading.Lock()
    finished = [0]
    t_start = time.time()
    print(f"{a.variant}: {len(cases)} cases × {a.reps} reps on {a.model}, {len(jobs)} to run, concurrency {a.concurrency}", flush=True)

    def one(job):
        c, k = job
        if a.regrade:
            return
        run = run_agent(c["prompt"], a.model, mcp_config, a.max_turns, a.timeout_s)
        with lock:
            if "error" in run:
                with errors_path.open("a") as f:
                    f.write(json.dumps({"prompt_id": c["id"], "rep": k, "class": "harness", "error": run["error"]}) + "\n")
                finished[0] += 1
                print(f"  {c['id']} rep{k}: ERROR {run['error'][:120]}", flush=True)
                return
        try:
            g, reason, sql = grade(c, run["spec"])
        except Exception as e:  # the grader itself failed: an error, not a zero
            with lock, errors_path.open("a") as f:
                f.write(json.dumps({"prompt_id": c["id"], "rep": k, "class": "grader", "error": str(e)[:500], "usage": run["usage"], "model": run["model"]}) + "\n")
            return
        row = {"prompt_id": c["id"], "rep": k, "prompt": c["prompt"], "tags": c["tags"], "grade": g,
               "explanation": {"pass": reason}, "model": run["model"], "latency_s": round(run["latency_s"], 2),
               "api_s": round(run["api_s"], 2), "cost_usd": run["cost_usd"], "tool_calls": run["tool_calls"],
               "sql_calls": run["sql_calls"], "tool_errors": run["tool_errors"], "turns": run["turns"],
               "usage": run["usage"], "status": "ok" if run["subtype"] == "success" else (run["subtype"] or "unknown"),
               "meta": {"spec_source": run["spec_source"], "candidate_spec": run["spec"], "candidate_sql": sql, "num_turns": run["num_turns"]}}
        with lock:
            with results_path.open("a") as f:
                f.write(json.dumps(row) + "\n")
            if c["id"] in train or os.environ.get("ZSQL_EVAL_ALL_TRACES"):
                (vdir / "traces" / f"{c['id']}_rep{k}.json").write_text(json.dumps(run["trace"], indent=1))
            finished[0] += 1
            el = time.time() - t_start
            print(f"  {finished[0]}/{len(jobs)} {c['id']} rep{k}: {'PASS' if g['pass'] else 'fail'} ({reason[:70]}) {run['latency_s']:.0f}s {run['tool_calls']} calls  ~{el/finished[0]*(len(jobs)-finished[0]):.0f}s left", flush=True)

    with ThreadPoolExecutor(max_workers=a.concurrency) as ex:
        list(ex.map(one, jobs))

    rows = [json.loads(l) for l in results_path.read_text().splitlines() if l.strip()]
    def score(ids):
        rs = [r for r in rows if r["prompt_id"] in ids]
        return (sum(r["grade"]["pass"] for r in rs) / len(rs)) if rs else float("nan"), len(rs)
    tr, ntr = score(train)
    te, nte = score(set(state["test_ids"]))
    cost = sum(r["cost_usd"] or 0 for r in rows)
    lat = sum(r["latency_s"] for r in rows) / len(rows) if rows else 0
    calls = sum(r["tool_calls"] for r in rows) / len(rows) if rows else 0
    print(f"{a.variant}: train {tr:.2f} ({ntr}) test {te:.2f} ({nte}) · {lat:.0f}s/case · {calls:.1f} calls/case · ${cost:.2f} total")

if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Imports golden cases from a roundhouse/strata-rs checkout.

Those fixtures were captured from strata's Rails planner tests and carry
both the Rails record dump and the query's spec. This service plans specs
only, so each case here is: the spec, the security context, the snapshot
it planned against, and the expected SQL or error. Cases the spec cannot
express (view segments with per-target links, linked filters as segment
criteria, static calculations, a segment planned directly) are left out,
as are the seven cases where Rails itself answers differently per branch.

    harness/import_goldens.py ../roundhouse/strata-rs <lossy-ids-file>

The lossy ids file lists one case id per line; roundhouse produces it with
`strata-rs golden --paths formed --dump DIR` (the *.spec-lossy.* files).
"""
import gzip, json, os, shutil, sys

src = sys.argv[1]
excluded = set()
if len(sys.argv) > 2:
    excluded = {l.strip() for l in open(sys.argv[2]) if l.strip()}
known = os.path.join(src, "harness/known_divergences.txt")
if os.path.exists(known):
    for l in open(known):
        l = l.strip()
        if l and not l.startswith("#"):
            excluded.add(l.split("|")[0].strip())

dst = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "fixtures", "golden")
for d in ("cases", "universes"):
    os.makedirs(os.path.join(dst, d), exist_ok=True)

FILTER_NOISE = {"id", "apply_to_all_views", "interactive", "interactive_required", "interactive_all_label",
                "interactive_control", "interactive_display_title", "type"}

def clean_filters(node):
    if isinstance(node, list):
        return [clean_filters(n) for n in node]
    if isinstance(node, dict):
        if "field" in node:
            out = {k: v for k, v in node.items() if k not in FILTER_NOISE}
            if isinstance(out.get("field"), dict):
                out["field"] = out["field"]["uid"]
            if isinstance(out.get("top_n_measure"), dict):
                out["top_n_measure"] = out["top_n_measure"]["uid"]
            for k in ("filter_value", "filter_value_end"):
                if k in out:
                    out[k.replace("filter_", "")] = out.pop(k)
            return {k: v for k, v in out.items() if v is not None}
        return {k: clean_filters(v) for k, v in node.items()}
    return node

def clean_spec(spec):
    spec = dict(spec)
    if "filters" in spec:
        spec["filters"] = clean_filters(spec["filters"])
    for seg in spec.get("segments") or []:
        if "filters" in seg:
            seg["filters"] = clean_filters(seg["filters"])
    for p in spec.get("projections", []):
        for d in p.get("decorators", []):
            for k in ("prompt", "share_full_result_set_with_ai", "id", "projection_id"):
                d.pop(k, None)
    return spec

def context(principal):
    if not principal:
        return None
    return {
        "email": principal.get("email"),
        "system_admin": bool(principal.get("sys_admin")),
        "project_admin": bool(principal.get("project_admin")),
        "tags": principal.get("tags") or [],
        "groups": [{"name": g.get("name"), "tags": g.get("tags") or []} for g in principal.get("groups") or []],
    }

used, total, kept = set(), 0, 0
for name in sorted(os.listdir(os.path.join(src, "fixtures/golden/cases"))):
    if not name.endswith(".jsonl"):
        continue
    out = []
    for line in open(os.path.join(src, "fixtures/golden/cases", name)):
        c = json.loads(line)
        total += 1
        spec = c.get("spec")
        if c["id"] in excluded or not spec or "__spec_error" in spec:
            continue
        case = {
            "id": c["id"],
            "variant": c["variant"],
            "universe": c["universe"],
            "context": context(c.get("principal")),
            "spec": clean_spec(spec),
            "expected": c["expected"],
        }
        if c.get("mutations"):
            case["mutations"] = c["mutations"]
        out.append(case)
        used.add(c["universe"])
        kept += 1
    if out:
        with open(os.path.join(dst, "cases", name), "w") as f:
            for case in out:
                f.write(json.dumps(case, sort_keys=True) + "\n")
for h in sorted(used):
    shutil.copy(os.path.join(src, "fixtures/golden/universes", h + ".json.gz"), os.path.join(dst, "universes", h + ".json.gz"))
manifest = json.load(open(os.path.join(src, "fixtures/golden/manifest.json")))
json.dump({"imported_from": {"strata_sha": manifest.get("strata_sha"), "cases": total}, "cases": kept, "universes": len(used)},
          open(os.path.join(dst, "manifest.json"), "w"), indent=2)
print(f"imported {kept} of {total} cases, {len(used)} universes")

//! A timing of data prep at the sizes it is for: a million-row upload,
//! a graph of steps, a join, edits and an output with checks, and the
//! previews a pipeline is built with. Ignored; run it with
//!
//! ```text
//! cargo test -p zsql-prep --release bench -- --ignored --nocapture
//! ```
//!
//! `ZSQL_PREP_BENCH_ROWS` sets the rows (1,000,000 by default) and
//! `ZSQL_PREP_BENCH_OUT` a file the run records are written to with
//! their times taken out, to compare what two builds say.

use super::*;
use serde_json::{json, Value};
use std::time::Instant;

fn sales_csv(rows: usize, region: &str) -> String {
    let regions = [" emea", "apac ", " amer ", "latam", "anz"];
    let notes = ["", "promo", "returned", "", "gift wrap", "promo code SPRING"];
    let mut s = String::with_capacity(rows * 64);
    s.push_str(&format!("id,{region},store,month,amount,qty,note\n"));
    for i in 0..rows {
        let r = regions[i % regions.len()];
        let store = (i * 7919) % 1000;
        let month = format!("2026-{:02}-01", i % 12 + 1);
        let amount = format!("{}.{:02}", (i * 31) % 5000, i % 100);
        let qty = (i * 13) % 20;
        let note = notes[(i / 3) % notes.len()];
        s.push_str(&format!("{i},{r},{store},{month},{amount},{qty},{note}\n"));
    }
    s
}

fn stores_csv() -> String {
    let mut s = String::from("store,city,owner\n");
    for i in 0..1000 {
        if i % 10 != 9 {
            s.push_str(&format!("{i},city {},owner {}\n", i % 97, i % 13));
        }
    }
    s
}

fn graph(sales: &str, stores: &str) -> Graph {
    let mut edits = vec![json!({ "id": "r1", "kind": "remap", "column": "region", "from": "LATAM", "to": "Latin America" })];
    for i in 0..20 {
        edits.push(json!({ "id": format!("s{i}"), "kind": "set", "column": "qty", "key": { "id": i * 4999 }, "to": 99, "was": 1 }));
    }
    edits.push(json!({ "id": "d1", "kind": "delete", "key": { "id": 7 } }));
    serde_json::from_value(json!({ "nodes": [
        { "id": "sales", "kind": "source", "source": sales },
        { "id": "stores", "kind": "source", "source": stores },
        { "id": "clean", "kind": "steps", "input": "sales", "steps": [
            { "op": "trim_val", "columns": ["region"] },
            { "op": "upper_case", "columns": ["region"] },
            { "op": "fill_empty", "column": "note", "value": "none" },
            { "op": "find_and_replace_val", "column": "note", "find": "promo", "with": "PROMO" },
            { "op": "add_column", "column": "total", "formula": "amount * qty" },
            { "op": "filter", "formula": "qty > 0" },
            { "op": "update_column", "column": "amount", "formula": "round(amount, 1)" },
        ]},
        { "id": "j", "kind": "join", "left": "clean", "right": "stores", "on": [{ "left": "store", "right": "store" }], "type": "left" },
        { "id": "fix", "kind": "edits", "input": "j", "key": ["id"], "edits": edits },
        { "id": "out", "kind": "output", "input": "fix", "key": ["id"], "checks": [
            { "check": "not_null", "columns": ["city"], "on_fail": "warn" },
        ]},
    ]}))
    .unwrap()
}

fn upload(t: &str) -> Upload<'_> {
    Upload { file_name: Some("sales.csv".into()), bytes: t.as_bytes(), options: CsvOptions::default() }
}

/// What a record says, without the times it took.
fn timeless(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for k in ["ms", "started", "ended", "taken", "made", "at", "age_secs", "id"] {
                if k == "id" {
                    if m.get("id").and_then(Value::as_str).is_some_and(|s| s.starts_with("bench-")) {
                        m.remove("id");
                    }
                    continue;
                }
                m.remove(k);
            }
            if let Some(Value::String(s)) = m.get_mut("why") {
                *s = s.split(" ago").next().unwrap_or_default().to_string();
            }
            for (_, x) in m.iter_mut() {
                timeless(x);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(timeless),
        Value::String(s) => {
            if let Some(i) = s.find(", taken ") {
                let end = s[i..].find(')').map_or(s.len(), |j| i + j);
                s.replace_range(i..end, "");
            }
        }
        _ => {}
    }
}

#[test]
#[ignore]
fn bench() {
    let rows: usize = std::env::var("ZSQL_PREP_BENCH_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/prep-bench");
    let dir = base.join(format!("{}-{}", std::process::id(), time::nanos()));
    std::fs::create_dir_all(&dir).unwrap();
    let prep = Prep::open(&dir, Settings::defaults(&dir)).unwrap();
    let scope = Scope { org: "default".into(), project: "bench".into() };
    let mut out: Vec<Value> = Vec::new();
    let mut times: Vec<(String, u128)> = Vec::new();
    let mut timed = |what: &str, t: Instant| {
        let ms = t.elapsed().as_millis();
        eprintln!("{what:<34} {ms:>7} ms");
        times.push((what.to_string(), ms));
    };

    let text = sales_csv(rows, "region");
    let t = Instant::now();
    let (sales, _) = prep.create_upload(&scope, Named::default(), upload(&text)).unwrap();
    timed("upload (new source)", t);
    let st = stores_csv();
    let (stores, _) = prep
        .create_upload(
            &scope,
            Named::default(),
            Upload { file_name: Some("stores.csv".into()), bytes: st.as_bytes(), options: CsvOptions::default() },
        )
        .unwrap();
    let d = prep.create_dataset(&scope, Named::default(), None).unwrap();
    let g = graph(&sales.id, &stores.id);

    for k in 0..2 {
        let t = Instant::now();
        let request = RunRequest { id: format!("bench-{k}"), dataset: d.id.clone(), graph: g.clone() };
        let r = match prep.begin(&scope, request, "ctx").unwrap() {
            Begun::Started(t) => prep.execute(*t),
            Begun::Answered(r) => *r,
        };
        timed(&format!("run {}", k + 1), t);
        assert_eq!(r.state, RunState::Succeeded, "{}", r.summary);
        for n in &r.nodes {
            eprintln!("    {:<8} {:>7} ms  {}", n.id, n.ms, n.summary);
        }
        let mut v = serde_json::to_value(&r).unwrap();
        timeless(&mut v);
        out.push(v);
    }

    let t = Instant::now();
    let page = prep.page(&scope, &d.id, None, 0, 100, &View::default()).unwrap();
    timed("page of the output", t);
    out.push(json!({ "page_total": page.total, "page": zsql_exec::json::rows(&page.rows).unwrap() }));

    for (node, whole) in [(None, false), (Some("clean"), false), (Some("j"), false), (Some("fix"), true)] {
        let t = Instant::now();
        let request = PreviewRequest { graph: g.clone(), node: node.map(str::to_string), whole, limit: None };
        let p = prep.preview(&scope, &request, "ctx", &HashMap::new()).unwrap();
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        timed(&format!("preview {} {}", node.unwrap_or("output"), if whole { "(whole)" } else { "(sample)" }), t);
        let mut v = json!({ "node": p.node, "nodes": p.nodes, "rows": zsql_exec::json::rows(&p.rows).unwrap() });
        timeless(&mut v);
        out.push(v);
    }

    // A new file where `region` is `area`: what reads it is refused, with
    // the column that took its values suggested.
    let renamed = sales_csv(rows, "area");
    let t = Instant::now();
    prep.add_upload(&scope, &sales.id, upload(&renamed)).unwrap();
    timed("upload (second snapshot)", t);
    let t = Instant::now();
    let request = PreviewRequest { graph: g.clone(), node: Some("clean".into()), whole: false, limit: None };
    let p = prep.preview(&scope, &request, "ctx", &HashMap::new()).unwrap();
    timed("preview refused (overlaps)", t);
    assert!(!p.problems.is_empty());
    out.push(serde_json::to_value(&p.problems).unwrap());
    let t = Instant::now();
    let c = prep.check(&scope, &g, "ctx").unwrap();
    timed("check refused (overlaps)", t);
    let mut v = serde_json::to_value(&c).unwrap();
    timeless(&mut v);
    out.push(v);

    eprintln!("total {} ms", times.iter().map(|(_, ms)| ms).sum::<u128>());
    if let Ok(path) = std::env::var("ZSQL_PREP_BENCH_OUT") {
        std::fs::write(path, serde_json::to_vec_pretty(&out).unwrap()).unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

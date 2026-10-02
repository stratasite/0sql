//! The HTTP client for zsqld, and the printing of what it answers.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub struct Client {
    pub server: String,
    pub api_key: Option<String>,
}

impl Client {
    pub fn new(server: &str, api_key: Option<String>) -> Self {
        Client { server: server.trim_end_matches('/').to_string(), api_key }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.server, path)
    }

    fn agent(&self) -> ureq::Agent {
        ureq::Agent::new_with_config(ureq::config::Config::builder().http_status_as_error(false).build())
    }

    fn auth<B>(&self, req: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        match &self.api_key {
            Some(k) => req.header("authorization", &format!("Bearer {k}")),
            None => req,
        }
    }

    /// Sends, and waits the server out if it says it has not started
    /// the request yet. The waiting is announced on stderr so it never
    /// lands in `--json` output.
    fn send(&self, what: &str, attempt: impl Fn() -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Value> {
        let mut waited = 0;
        loop {
            let r = attempt().with_context(|| what.to_string())?;
            match retry_after(&r) {
                Some(secs) if waited < RETRIES => {
                    waited += 1;
                    eprintln!("{what}: the server is at its limit, waiting {secs}s ({waited} of {RETRIES})");
                    std::thread::sleep(std::time::Duration::from_secs(secs));
                }
                _ => return read(r),
            }
        }
    }

    pub fn get(&self, path: &str) -> Result<Value> {
        let url = self.url(path);
        self.send(&format!("GET {url}"), || self.auth(self.agent().get(&url)).call())
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let url = self.url(path);
        self.send(&format!("POST {url}"), || self.auth(self.agent().post(&url)).send_json(body))
    }

    pub fn post_bytes(&self, path: &str, content_type: &str, bytes: &[u8]) -> Result<Value> {
        let url = self.url(path);
        self.send(&format!("POST {url}"), || self.auth(self.agent().post(&url)).header("content-type", content_type).send(bytes))
    }

    pub fn delete(&self, path: &str) -> Result<Value> {
        let url = self.url(path);
        self.send(&format!("DELETE {url}"), || self.auth(self.agent().delete(&url)).call())
    }
}

/// How many times a refusal is worth waiting out, and the longest the
/// CLI will sit on one wait.
const RETRIES: u32 = 3;
const LONGEST_WAIT: u64 = 30;

/// Whether to wait and ask again, and for how long.
///
/// A `429`, and a `503` that names a delay, are the server saying it
/// refused the request before running any of it: asking again is safe
/// whatever the request was, including a deploy. A `503` with no delay
/// is a deadline that passed inside the handler, which may have done
/// part of the work, so that one is reported rather than repeated.
fn retry_after(r: &ureq::http::Response<ureq::Body>) -> Option<u64> {
    let named = r.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    match (r.status().as_u16(), named) {
        (429, secs) => Some(secs.unwrap_or(1).clamp(1, LONGEST_WAIT)),
        (503, Some(secs)) => Some(secs.clamp(1, LONGEST_WAIT)),
        _ => None,
    }
}

fn read(mut r: ureq::http::Response<ureq::Body>) -> Result<Value> {
    let status = r.status();
    let text = r.body_mut().read_to_string()?;
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text.clone()));
    if !status.is_success() {
        if let Some(e) = value.get("error") {
            bail!("{}: {}", e["class"].as_str().unwrap_or("error"), e["message"].as_str().unwrap_or(&text));
        }
        let brief: String = text.chars().take(200).collect::<String>().replace('\n', " ");
        bail!("server answered {status}: {brief}{}", if text.len() > 200 { "..." } else { "" });
    }
    Ok(value)
}

pub fn branch_path(uid: &str, branch: &str) -> String {
    format!("/projects/{}/branches/{}", urlencode(uid), urlencode(branch))
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `/sql`: `{sql, adapter}`; `/explain`: that plus timings, phases and the node graph.
pub fn sql(client: &Client, uid: &str, branch: &str, spec: Value, context: Option<&Value>, explain: bool) -> Result<Value> {
    let mut body = json!({ "spec": spec });
    if let Some(c) = context {
        body["context"] = c.clone();
    }
    client.post(&format!("{}/{}", branch_path(uid, branch), if explain { "explain" } else { "sql" }), &body)
}

/// `/explore`: the dimensions and measures the query can take on, narrowed
/// by a search term.
pub fn explore(client: &Client, uid: &str, branch: &str, spec: Value, context: Option<&Value>, q: Option<&str>) -> Result<Value> {
    let mut body = json!({ "spec": spec });
    if let Some(c) = context {
        body["context"] = c.clone();
    }
    if let Some(q) = q.map(str::trim).filter(|q| !q.is_empty()) {
        body["q"] = json!(q);
    }
    client.post(&format!("{}/explore", branch_path(uid, branch)), &body)
}

pub fn print_explore(v: &Value, style: &dyn Fn(&str) -> String) {
    let mut counts = Vec::new();
    for (key, label) in [("dimensions", "dim"), ("measures", "msr")] {
        let fields = v[key].as_array().cloned().unwrap_or_default();
        for f in &fields {
            let tables: Vec<&str> = f["tables"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let near = match f["score"].as_f64() {
                Some(s) if s < 1.0 => format!(" ~{s:.2}"),
                _ => String::new(),
            };
            println!("{:<40} {} {:<32} {}{near}", f["uid"].as_str().unwrap_or(""), label, f["name"].as_str().unwrap_or(""), style(&tables.join(", ")));
        }
        counts.push(format!("{} {key}", fields.len()));
    }
    println!("{}", style(&format!("-- can add {} · server {} us", counts.join(", "), v["total_us"])));
}

/// A dimension's sampled values by uid or name, else by a unique substring
/// match over the deployment's dimensions.
pub fn values_for(client: &Client, uid: &str, branch: &str, dim: &str) -> Result<Value> {
    if let Ok(v) = client.get(&format!("{}/values/{}", branch_path(uid, branch), urlencode(dim))) {
        return Ok(v);
    }
    let fields = client.get(&format!("{}/fields?q={}&hidden=true", branch_path(uid, branch), urlencode(dim)))?;
    let dims: Vec<String> = fields["fields"].as_array().into_iter().flatten().filter(|f| f["kind"] == "dimension").filter_map(|f| f["uid"].as_str().map(str::to_string)).collect();
    match dims.as_slice() {
        [one] => client.get(&format!("{}/values/{}", branch_path(uid, branch), urlencode(one))),
        [] => bail!("no dimension matches '{dim}'"),
        many => bail!("'{dim}' matches several dimensions: {}", many.join(", ")),
    }
}

// ---------------------------------------------------------------- printing

pub fn print_json(v: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// The explain answer as text: the SQL, the node graph from the root down,
/// and what each phase cost.
/// The line above every statement: which datasource runs it. A SQL
/// comment, so the output stays pasteable into a client.
pub fn datasource_line(v: &Value) -> String {
    format!("-- datasource: {}", v["datasource"].as_str().unwrap_or("?"))
}

pub fn print_explain(v: &Value, style: &dyn Fn(&str) -> String) {
    for c in corrections(v) {
        println!("{}", style(&c));
    }
    println!("{}", style(&datasource_line(v)));
    println!("{}", v["sql"].as_str().unwrap_or(""));
    let nodes = v["nodes"].as_array().cloned().unwrap_or_default();
    if nodes.is_empty() {
        return;
    }
    println!();
    println!("{}", style(&format!("-- {} nodes", nodes.len())));
    fn walk(n: &Value, depth: usize, via: &str, nodes: &[Value], style: &dyn Fn(&str) -> String) {
        let by_id = |id: u64| nodes.iter().find(|n| n["id"].as_u64() == Some(id));
        let pad = "  ".repeat(depth);
        let mut head = format!("{pad}{via}{} {}", n["kind"].as_str().unwrap_or("?"), n["alias"].as_str().unwrap_or(""));
        if let Some(t) = n["table"].as_str() {
            head.push_str(&format!(" on {t}"));
            if let Some(d) = n["datasource"].as_str() {
                head.push_str(&format!(" ({d})"));
            }
        }
        if let Some(j) = n["join_type"].as_str() {
            head.push_str(&format!(" {j} join"));
        }
        if n["group_by"].as_bool() == Some(true) {
            head.push_str(" group by");
        }
        if n["segment"].as_bool() == Some(true) {
            head.push_str(" [segment]");
        }
        if let Some(t) = n["transform"].as_str() {
            head.push_str(&format!(" [{t}]"));
        }
        if let Some(p) = n["purpose"].as_str() {
            head.push_str(&format!(" [{p}]"));
        }
        println!("{head}");
        let list = |label: &str, key: &str| {
            let items: Vec<&str> = n[key].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            if !items.is_empty() {
                println!("{pad}    {}", style(&format!("{label}: {}", items.join(", "))));
            }
        };
        list("paths", "paths");
        list("projections", "projections");
        list("filters", "filters");
        list("security", "security_filters");
        list("masks", "security_masks");
        for s in n["strategies"].as_array().into_iter().flatten() {
            if let Some(c) = s["node"].as_u64().and_then(by_id) {
                walk(c, depth + 1, &format!("{} <- ", s["kind"].as_str().unwrap_or("strategy")), nodes, style);
            }
        }
        for c in n["inputs"].as_array().into_iter().flatten().filter_map(Value::as_u64).filter_map(by_id) {
            walk(c, depth + 1, "", nodes, style);
        }
    }
    if let Some(root) = nodes.iter().find(|n| n["root"].as_bool() == Some(true)) {
        walk(root, 0, "", &nodes, style);
    }
    let phases: Vec<String> = v["phases"].as_array().into_iter().flatten().map(|p| format!("{} {}", p["name"].as_str().unwrap_or(""), p["us"])).collect();
    println!();
    println!("{}", style(&format!("-- server {} us: parser {} us, plan {} us", v["timings"]["total_us"], v["timings"]["parser_us"], v["timings"]["plan_us"])));
    println!("{}", style(&format!("-- phases (us): {}", phases.join(", "))));
}

pub fn print_fields(v: &Value) {
    for f in v["fields"].as_array().into_iter().flatten() {
        let kind = match f["kind"].as_str() {
            Some("dimension") => "dim",
            Some("measure") => "msr",
            _ => "?",
        };
        let near = match f["score"].as_f64() {
            Some(s) if s < 1.0 => format!(" ~{s:.2}"),
            _ => String::new(),
        };
        println!("{:<40} {} {}{}{near}", f["uid"].as_str().unwrap_or(""), kind, f["name"].as_str().unwrap_or(""), if f["hidden"].as_bool().unwrap_or(false) { " (hidden)" } else { "" });
    }
}

/// One line per reference the server resolved by near match.
pub fn corrections(v: &Value) -> Vec<String> {
    v["corrections"].as_array().into_iter().flatten().map(|c| format!("{} → {} (~{})", c["term"].as_str().unwrap_or(""), c["field_name"].as_str().unwrap_or(""), c["score"])).collect()
}

pub fn print_tables(v: &Value) {
    for t in v["tables"].as_array().into_iter().flatten() {
        println!("{:<32} {:<32} {} cost {}", t["uid"].as_str().unwrap_or(""), t["physical_name"].as_str().unwrap_or(""), t["datasource"].as_str().unwrap_or("?"), t["cost"]);
    }
}

pub fn print_values(v: &Value) {
    if let Some(r) = v.get("range") {
        println!("{:<40} {} .. {}  (distinct {})", v["uid"].as_str().unwrap_or(""), r["min"].as_str().unwrap_or(""), r["max"].as_str().unwrap_or(""), v["distinct"]);
        return;
    }
    let values = v["values"].as_array().cloned().unwrap_or_default();
    println!("{:<40} {} of {} values", v["uid"].as_str().unwrap_or(""), values.len(), v["distinct"]);
    for x in values.iter().take(20) {
        println!("    {:<60} {}", x["v"].as_str().unwrap_or(""), x["n"]);
    }
    if values.len() > 20 {
        println!("    ... {} more", values.len() - 20);
    }
}

/// The deploy or validate answer: counts, warnings, test results.
pub fn print_deploy(v: &Value) -> bool {
    let validated = v["validated_only"].as_bool().unwrap_or(false);
    println!(
        "{} {}/{}: {} tables, {} fields, {} joins, {} paths, {} policies, {} sampled dimensions",
        if validated { "validated" } else { "deployed" },
        v["project"].as_str().unwrap_or(""),
        v["branch"].as_str().unwrap_or(""),
        v["tables"],
        v["fields"],
        v["joins"],
        v["paths"],
        v["policies"],
        v["values"]
    );
    for w in v["warnings"].as_array().into_iter().flatten() {
        println!("warning: {}", w.as_str().unwrap_or(""));
    }
    print_tests(&v["test_results"])
}

/// Prints test results; true when none failed.
pub fn print_tests(t: &Value) -> bool {
    let results = t["results"].as_array().cloned().unwrap_or_default();
    if results.is_empty() {
        if t.is_object() {
            println!("no tests deployed (tests/*.yml)");
        }
        return true;
    }
    for r in &results {
        let status = r["status"].as_str().unwrap_or("?");
        println!("{:<6} {}", status.to_uppercase(), r["name"].as_str().unwrap_or(""));
        if status != "passed" {
            if let Some(m) = r["message"].as_str() {
                println!("       {m}");
            }
            if let Some(sql) = r["generated_sql"].as_str() {
                println!("       --- generated\n{}", indent(sql));
            }
            if let Some(sql) = r["expected_sql"].as_str() {
                println!("       --- expected\n{}", indent(sql));
            }
        }
    }
    println!("tests: {} passed, {} failed", t["passed"], t["failed"]);
    t["failed"].as_u64().unwrap_or(0) == 0
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("       {l}")).collect::<Vec<_>>().join("\n")
}

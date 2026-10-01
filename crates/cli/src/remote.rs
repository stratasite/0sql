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

    pub fn get(&self, path: &str) -> Result<Value> {
        let r = self.auth(self.agent().get(self.url(path))).call().with_context(|| format!("GET {}", self.url(path)))?;
        read(r)
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let r = self.auth(self.agent().post(self.url(path))).send_json(body).with_context(|| format!("POST {}", self.url(path)))?;
        read(r)
    }

    pub fn post_bytes(&self, path: &str, content_type: &str, bytes: &[u8]) -> Result<Value> {
        let r = self.auth(self.agent().post(self.url(path))).header("content-type", content_type).send(bytes).with_context(|| format!("POST {}", self.url(path)))?;
        read(r)
    }

    pub fn delete(&self, path: &str) -> Result<Value> {
        let r = self.auth(self.agent().delete(self.url(path))).call().with_context(|| format!("DELETE {}", self.url(path)))?;
        read(r)
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
pub fn print_explain(v: &Value, style: &dyn Fn(&str) -> String) {
    for c in corrections(v) {
        println!("{}", style(&c));
    }
    println!("{}", v["sql"].as_str().unwrap_or(""));
    let nodes = v["nodes"].as_array().cloned().unwrap_or_default();
    if nodes.is_empty() {
        return;
    }
    println!();
    println!("{}", style(&format!("-- {} · {} nodes", v["adapter"].as_str().unwrap_or(""), nodes.len())));
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

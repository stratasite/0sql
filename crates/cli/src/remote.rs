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

pub fn plan(client: &Client, uid: &str, branch: &str, spec: Value, context: Option<&Value>, explain: bool) -> Result<Value> {
    let mut body = json!({ "spec": spec, "explain": explain });
    if let Some(c) = context {
        body["context"] = c.clone();
    }
    client.post(&format!("{}/plan", branch_path(uid, branch)), &body)
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

pub fn print_fields(v: &Value) {
    for f in v["fields"].as_array().into_iter().flatten() {
        let kind = match f["kind"].as_str() {
            Some("dimension") => "dim",
            Some("measure") => "msr",
            _ => "?",
        };
        println!("{:<40} {} {}{}", f["uid"].as_str().unwrap_or(""), kind, f["name"].as_str().unwrap_or(""), if f["hidden"].as_bool().unwrap_or(false) { " (hidden)" } else { "" });
    }
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

//! The HTTP client for a server. What it answers is printed by
//! `show.rs`, which prints a local answer the same way.

use crate::failure::Refusal;
use crate::side::Query;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub struct Client {
    pub server: String,
    pub api_key: Option<String>,
    /// For every call but a deploy: a server that accepts and never
    /// answers must not hang the command.
    agent: ureq::Agent,
    /// For a deploy, which unpacks, loads and tests a whole model.
    slow: ureq::Agent,
}

impl Client {
    pub fn new(server: &str, api_key: Option<String>) -> Self {
        let agent = |whole: u64| {
            ureq::Agent::new_with_config(
                ureq::config::Config::builder()
                    .http_status_as_error(false)
                    .timeout_connect(Some(std::time::Duration::from_secs(10)))
                    .timeout_global(Some(std::time::Duration::from_secs(whole)))
                    .build(),
            )
        };
        Client { server: server.trim_end_matches('/').to_string(), api_key, agent: agent(60), slow: agent(600) }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.server, path)
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
        self.send(&format!("GET {url}"), || self.auth(self.agent.get(&url)).call())
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let url = self.url(path);
        self.send(&format!("POST {url}"), || self.auth(self.agent.post(&url)).send_json(body))
    }

    pub fn post_bytes(&self, path: &str, content_type: &str, bytes: &[u8]) -> Result<Value> {
        let url = self.url(path);
        self.send(&format!("POST {url}"), || self.auth(self.slow.post(&url)).header("content-type", content_type).send(bytes))
    }

    pub fn delete(&self, path: &str) -> Result<Value> {
        let url = self.url(path);
        self.send(&format!("DELETE {url}"), || self.auth(self.agent.delete(&url)).call())
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

/// The answer, or the server's refusal with its class kept, so it is
/// reported exactly as a local refusal would be.
fn read(mut r: ureq::http::Response<ureq::Body>) -> Result<Value> {
    let status = r.status();
    let text = r.body_mut().read_to_string()?;
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text.clone()));
    if !status.is_success() {
        if let Some(e) = value.get("error") {
            return Err(Refusal {
                class: e["class"].as_str().unwrap_or("error").to_string(),
                message: e["message"].as_str().unwrap_or(&text).to_string(),
            }
            .into());
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

/// The body of a planning request: the spec, or the shorthand line
/// for the server to read, and the context.
fn query_body(query: &Query, context: Option<&Value>) -> Value {
    let mut body = match query {
        Query::Spec(s) => json!({ "spec": s }),
        Query::Expr(e) => json!({ "expr": e }),
    };
    if let Some(c) = context {
        body["context"] = c.clone();
    }
    body
}

/// `/sql`: `{sql, adapter}`; `/explain`: that plus timings, phases and the node graph.
pub fn sql(client: &Client, uid: &str, branch: &str, query: &Query, context: Option<&Value>, explain: bool) -> Result<Value> {
    client.post(&format!("{}/{}", branch_path(uid, branch), if explain { "explain" } else { "sql" }), &query_body(query, context))
}

/// `/explore`: the dimensions and measures the query can take on, narrowed
/// by a search term.
pub fn explore(client: &Client, uid: &str, branch: &str, query: &Query, context: Option<&Value>, q: Option<&str>) -> Result<Value> {
    let mut body = query_body(query, context);
    if let Some(q) = q.map(str::trim).filter(|q| !q.is_empty()) {
        body["q"] = json!(q);
    }
    client.post(&format!("{}/explore", branch_path(uid, branch)), &body)
}

/// A dimension's sampled values by uid, name or unique substring; the
/// server resolves the name.
pub fn values_for(client: &Client, uid: &str, branch: &str, dim: &str) -> Result<Value> {
    client.get(&format!("{}/values/{}", branch_path(uid, branch), urlencode(dim)))
}

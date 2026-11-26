//! The HTTP client for a server. What it answers is printed by
//! `show.rs`, which prints a local answer the same way.

use crate::failure::Refusal;
use crate::side::Query;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub struct Client {
    pub server: String,
    /// For every call but a deploy: a server that accepts and never
    /// answers must not hang the command.
    agent: ureq::Agent,
    /// For a deploy, which unpacks, loads and tests a whole model.
    slow: ureq::Agent,
}

impl Client {
    pub fn new(server: &str) -> Self {
        let agent = |whole: u64| {
            ureq::Agent::new_with_config(
                ureq::config::Config::builder()
                    .http_status_as_error(false)
                    .timeout_connect(Some(std::time::Duration::from_secs(10)))
                    .timeout_global(Some(std::time::Duration::from_secs(whole)))
                    .build(),
            )
        };
        Client { server: server.trim_end_matches('/').to_string(), agent: agent(60), slow: agent(600) }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.server, path)
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
        self.send(&format!("GET {url}"), || self.agent.get(&url).call())
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let url = self.url(path);
        self.send(&format!("POST {url}"), || self.agent.post(&url).send_json(body))
    }

    pub fn post_bytes(&self, path: &str, content_type: &str, bytes: &[u8]) -> Result<Value> {
        let url = self.url(path);
        self.send(&format!("POST {url}"), || self.slow.post(&url).header("content-type", content_type).send(bytes))
    }

    /// A deploy or a validation told as it goes: each step the server
    /// sends is handed to `on` as it arrives, and the answer is the last
    /// line's. A server from before the stream answers whole, and is read
    /// as it always was, with no steps.
    pub fn post_bytes_streamed(&self, path: &str, content_type: &str, bytes: &[u8], on: &mut dyn FnMut(&Value)) -> Result<Value> {
        let url = self.url(path);
        let what = format!("POST {url}");
        let mut waited = 0;
        let r = loop {
            let r = self
                .slow
                .post(&url)
                .header("content-type", content_type)
                .header("accept", NDJSON)
                .send(bytes)
                .with_context(|| what.clone())?;
            match retry_after(&r) {
                Some(secs) if waited < RETRIES => {
                    waited += 1;
                    eprintln!("{what}: the server is at its limit, waiting {secs}s ({waited} of {RETRIES})");
                    std::thread::sleep(std::time::Duration::from_secs(secs));
                }
                _ => break r,
            }
        };
        let streamed = r.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with(NDJSON));
        if !streamed || !r.status().is_success() {
            return read(r);
        }
        read_stream(std::io::BufReader::new(r.into_body().into_reader()), on).with_context(|| what.clone())
    }

    pub fn delete(&self, path: &str) -> Result<Value> {
        let url = self.url(path);
        self.send(&format!("DELETE {url}"), || self.agent.delete(&url).call())
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

/// The media type of a deploy told as it goes: one JSON object a line.
pub const NDJSON: &str = "application/x-ndjson";

/// Reads a streamed deploy to its last line: `done` is the answer,
/// `error` the refusal as a whole answer would have said it, and any
/// other line a step for `on`. A stream that stops before either is a
/// deploy whose end nobody heard, and is said to be one.
fn read_stream(reader: impl std::io::BufRead, on: &mut dyn FnMut(&Value)) -> Result<Value> {
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line).with_context(|| format!("a line the server sent: {line}"))?;
        match v["event"].as_str() {
            Some("done") => return Ok(v["result"].clone()),
            Some("error") => {
                let e = &v["error"];
                return Err(Refusal {
                    class: e["class"].as_str().unwrap_or("error").to_string(),
                    message: e["message"].as_str().unwrap_or("").to_string(),
                }
                .into());
            }
            _ => on(&v),
        }
    }
    bail!("the server stopped before it said how the deploy ended; `zsql deployments` says what is live")
}

/// What every path of an org starts with: `/orgs/{org}`, and nothing
/// for the default org, whose paths are the ones a server had before
/// it kept orgs.
pub fn org_path(org: Option<&str>) -> String {
    match org.map(str::trim).filter(|o| !o.is_empty() && *o != DEFAULT_ORG) {
        Some(org) => format!("/orgs/{}", urlencode(org)),
        None => String::new(),
    }
}

/// The org a project with no `org:` is deployed under.
pub const DEFAULT_ORG: &str = "default";

pub fn project_path(org: Option<&str>, uid: &str) -> String {
    format!("{}/projects/{}", org_path(org), urlencode(uid))
}

pub fn branch_path(org: Option<&str>, uid: &str, branch: &str) -> String {
    format!("{}/projects/{}/branches/{}", org_path(org), urlencode(uid), urlencode(branch))
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

/// The body of a planning request: the spec, or the expression line
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
pub fn sql(
    client: &Client,
    org: Option<&str>,
    uid: &str,
    branch: &str,
    query: &Query,
    context: Option<&Value>,
    explain: bool,
) -> Result<Value> {
    client.post(&format!("{}/{}", branch_path(org, uid, branch), if explain { "explain" } else { "sql" }), &query_body(query, context))
}

/// `/explore`: the dimensions and measures the query can take on, narrowed
/// by a search term.
pub fn explore(
    client: &Client,
    org: Option<&str>,
    uid: &str,
    branch: &str,
    query: &Query,
    context: Option<&Value>,
    q: Option<&str>,
) -> Result<Value> {
    let mut body = query_body(query, context);
    if let Some(q) = q.map(str::trim).filter(|q| !q.is_empty()) {
        body["q"] = json!(q);
    }
    client.post(&format!("{}/explore", branch_path(org, uid, branch)), &body)
}

/// A dimension's sampled values by uid, name or unique substring; the
/// server resolves the name.
pub fn values_for(client: &Client, org: Option<&str>, uid: &str, branch: &str, dim: &str) -> Result<Value> {
    client.get(&format!("{}/values/{}", branch_path(org, uid, branch), urlencode(dim)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(text: &str) -> (Result<Value>, Vec<String>) {
        let mut seen = Vec::new();
        let got = read_stream(std::io::Cursor::new(text.to_string()), &mut |v| seen.push(v["event"].as_str().unwrap_or("").to_string()));
        (got, seen)
    }

    /// The steps go to the caller as they come; the last line is the answer.
    #[test]
    fn a_stream_is_its_steps_then_its_answer() {
        let (got, seen) = read_all("{\"event\":\"unpacked\"}\n\n{\"event\":\"someday\"}\n{\"event\":\"done\",\"result\":{\"tables\":3}}\n");
        assert_eq!(got.unwrap()["tables"], 3);
        assert_eq!(seen, ["unpacked", "someday"]);
    }

    /// A refusal on the stream is the refusal a whole answer would be.
    #[test]
    fn a_streamed_refusal_keeps_its_class() {
        let (got, _) =
            read_all("{\"event\":\"error\",\"status\":400,\"error\":{\"class\":\"DeployError\",\"message\":\"no project.yml\"}}\n");
        let e = got.unwrap_err();
        let refusal = e.downcast_ref::<Refusal>().unwrap();
        assert_eq!((refusal.class.as_str(), refusal.message.as_str()), ("DeployError", "no project.yml"));
    }

    /// A stream cut off before its end says so, rather than succeeding.
    #[test]
    fn a_stream_with_no_end_is_an_error() {
        let (got, seen) = read_all("{\"event\":\"unpacked\"}\n");
        assert!(got.unwrap_err().to_string().contains("stopped before"));
        assert_eq!(seen, ["unpacked"]);
    }
}

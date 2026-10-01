//! Commands against a running zsqld: the same deploy, plan and discovery
//! the local commands do, over HTTP. The shorthand line is parsed here and
//! sent as a spec, so the server never sees the shorthand.

use crate::exprs;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const DEFAULT_SERVER: &str = "http://127.0.0.1:3699";

pub struct Client {
    pub server: String,
}

impl Client {
    pub fn new(server: &str) -> Self {
        Client { server: server.trim_end_matches('/').to_string() }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.server, path)
    }

    /// GET; a non-2xx answer is returned as the server's error body.
    pub fn get(&self, path: &str) -> Result<Value> {
        let r = ureq::get(self.url(path)).config().http_status_as_error(false).build().call().with_context(|| format!("GET {}", self.url(path)))?;
        read(r)
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let r = ureq::post(self.url(path)).config().http_status_as_error(false).build().send_json(body).with_context(|| format!("POST {}", self.url(path)))?;
        read(r)
    }

    pub fn post_bytes(&self, path: &str, content_type: &str, bytes: &[u8]) -> Result<Value> {
        let r = ureq::post(self.url(path)).config().http_status_as_error(false).build().header("content-type", content_type).send(bytes).with_context(|| format!("POST {}", self.url(path)))?;
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
        bail!("server answered {status}: {text}");
    }
    Ok(value)
}

/// The project uid: given, or read from project.yml in the given directory.
pub fn project_uid(uid: Option<String>, project: Option<&Path>) -> Result<String> {
    if let Some(u) = uid {
        return Ok(u);
    }
    let dir = project.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let text = std::fs::read_to_string(dir.join("project.yml")).with_context(|| format!("reading {}; give --uid or --project", dir.join("project.yml").display()))?;
    let v: serde_yaml::Value = serde_yaml::from_str(&text)?;
    v.get("uid")
        .and_then(|u| u.as_str())
        .map(str::to_string)
        .or_else(|| v.get("name").and_then(|n| n.as_str()).map(zsql_project::parameterize))
        .ok_or_else(|| anyhow::anyhow!("project.yml has no uid or name"))
}

fn branch_path(uid: &str, branch: &str) -> String {
    format!("/projects/{uid}/branches/{branch}")
}

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

/// A dimension's sampled values by uid or name, else by a unique substring
/// match over the deployment's dimensions, as the local `.values` does.
pub fn values_for(client: &Client, uid: &str, branch: &str, dim: &str) -> Result<Value> {
    let direct = client.get(&format!("{}/values/{}", branch_path(uid, branch), urlencode(dim)));
    if let Ok(v) = direct {
        return Ok(v);
    }
    let fields = client.get(&format!("{}/fields?q={}&hidden=true", branch_path(uid, branch), urlencode(dim)))?;
    let dims: Vec<String> = fields["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| f["kind"] == "dimension")
        .filter_map(|f| f["uid"].as_str().map(str::to_string))
        .collect();
    match dims.as_slice() {
        [one] => client.get(&format!("{}/values/{}", branch_path(uid, branch), urlencode(one))),
        [] => bail!("no dimension matches '{dim}'"),
        many => bail!("'{dim}' matches several dimensions: {}", many.join(", ")),
    }
}

/// Plans a spec on the server and prints the SQL with the server's timings.
pub fn plan(client: &Client, uid: &str, branch: &str, spec: Value, context: Option<&Value>, explain: bool) -> Result<Value> {
    let mut body = json!({ "spec": spec, "explain": explain });
    if let Some(c) = context {
        body["context"] = c.clone();
    }
    client.post(&format!("{}/plan", branch_path(uid, branch)), &body)
}

/// The interactive loop against a server: same commands as the local
/// repl, every answer from the endpoints.
pub fn repl(client: &Client, uid: &str, branch: &str, context: Option<Value>) -> Result<()> {
    let tty = std::io::stdout().is_terminal();
    let dim = |s: &str| if tty { format!("\x1b[2m{s}\x1b[0m") } else { s.to_string() };
    let bold = |s: &str| if tty { format!("\x1b[1m{s}\x1b[0m") } else { s.to_string() };
    let summary = client.get(&branch_path(uid, branch))?;
    println!("{}", dim(&format!("{} / {} on {}: {} tables, {} fields, {} joins, {} policies", uid, branch, client.server, summary["tables"], summary["fields"], summary["joins"], summary["policies"])));
    println!("{}", dim("type a line of projections and filters; .help for the forms, .quit to leave"));
    let field_names: Vec<String> = client
        .get(&format!("{}/fields?hidden=true", branch_path(uid, branch)))?["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|f| [f["uid"].as_str().unwrap_or("").to_lowercase(), f["name"].as_str().unwrap_or("").to_lowercase()])
        .collect();

    let mut editor: Option<rustyline::DefaultEditor> = if std::io::stdin().is_terminal() { rustyline::DefaultEditor::new().ok() } else { None };
    let history = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".zsql_history"));
    if let (Some(ed), Some(h)) = (editor.as_mut(), &history) {
        let _ = ed.load_history(h);
    }
    let stdin = std::io::stdin();
    loop {
        let line = match editor.as_mut() {
            Some(ed) => match ed.readline("zsql> ") {
                Ok(l) => {
                    let _ = ed.add_history_entry(l.as_str());
                    l
                }
                Err(rustyline::error::ReadlineError::Interrupted) => continue,
                Err(_) => break,
            },
            None => {
                print!("zsql> ");
                std::io::stdout().flush()?;
                let mut l = String::new();
                if stdin.lock().read_line(&mut l)? == 0 {
                    break;
                }
                l
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (word, rest) = line.split_once(' ').map(|(c, r)| (c, r.trim())).unwrap_or((line, ""));
        let names_a_field = |w: &str| field_names.iter().any(|n| n == &w.to_lowercase());
        let cmd = match word.strip_prefix('.') {
            Some(c) => c,
            None if matches!(word, "help" | "?" | "quit" | "exit" | "q") && rest.is_empty() && !names_a_field(word) => word,
            None => "",
        };
        let outcome: Result<()> = (|| {
            match cmd {
                "quit" | "exit" | "q" => return Ok(()),
                "help" | "?" => println!("{}", exprs::HELP),
                "tables" => print_tables(&client.get(&format!("{}/tables", branch_path(uid, branch)))?),
                "fields" => print_fields(&client.get(&format!("{}/fields?q={}", branch_path(uid, branch), urlencode(rest)))?),
                "values" => {
                    if rest.is_empty() {
                        println!("usage: .values <dimension>");
                    } else {
                        print_values(&values_for(client, uid, branch, rest)?);
                    }
                }
                "tojson" => match exprs::parse_line(rest) {
                    Ok((spec, items)) => {
                        for d in exprs::describe(&items) {
                            println!("{}", dim(&d));
                        }
                        println!("{}", serde_json::to_string_pretty(&spec.to_value())?);
                    }
                    Err(e) => println!("error: {e}"),
                },
                "" | "json" => {
                    let json_text = if cmd == "json" { Some(rest) } else if line.starts_with('{') { Some(line) } else { None };
                    let spec: Result<Value, String> = match json_text {
                        Some(text) => serde_json::from_str(text).map_err(|e| e.to_string()),
                        None => exprs::parse_line(line).map(|(spec, items)| {
                            for d in exprs::describe(&items) {
                                println!("{}", dim(&d));
                            }
                            spec.to_value()
                        }),
                    };
                    match spec {
                        Err(e) => println!("error: {e}"),
                        Ok(spec) => {
                            let t0 = Instant::now();
                            match plan(client, uid, branch, spec, context.as_ref(), false) {
                                Ok(v) => {
                                    println!("{}", bold(v["sql"].as_str().unwrap_or("")));
                                    println!(
                                        "{}",
                                        dim(&format!(
                                            "-- {} · round trip {:.2} ms (server lower {} us, plan {} us)",
                                            v["adapter"].as_str().unwrap_or(""),
                                            t0.elapsed().as_secs_f64() * 1e3,
                                            v["timings_us"]["lower"],
                                            v["timings_us"]["plan"]
                                        ))
                                    );
                                }
                                Err(e) => println!("error: {e}"),
                            }
                        }
                    }
                }
                other => println!("unknown command .{other}; .help lists them"),
            }
            Ok(())
        })();
        if matches!(cmd, "quit" | "exit" | "q") {
            break;
        }
        if let Err(e) = outcome {
            println!("error: {e}");
        }
    }
    if let (Some(ed), Some(h)) = (editor.as_mut(), &history) {
        let _ = ed.save_history(h);
    }
    Ok(())
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

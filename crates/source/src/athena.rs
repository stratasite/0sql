//! Athena over its JSON API, signed with SigV4: `StartQueryExecution`,
//! `GetQueryExecution` polled until the state settles, `GetQueryResults`
//! paged by token. Every cell arrives as a `VarCharValue`, and the first
//! row of a `SELECT`'s first page repeats the column names, which is
//! dropped when it does.
//!
//! The credentials are the first three links of the chain every AWS tool
//! walks: explicit keys in `.zsql`, the `AWS_*` variables, a profile in
//! the shared credentials file. No instance metadata and no SSO cache
//! until someone needs them. The region follows the same order, ending
//! in the shared config file.

use crate::{http, read_only, sigv4, Connection, Result, Rows, Secret, Secrets, Source, SourceError};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const USER_AGENT: &str = concat!("zsql/", env!("CARGO_PKG_VERSION"));
/// Rows per `GetQueryResults` page: the API's own maximum.
const PAGE: usize = 1000;

pub struct Athena {
    agent: ureq::Agent,
    base: String,
    host: String,
    conn: Connection,
    region: String,
    creds: sigv4::Credentials,
    catalog: String,
    database: Option<String>,
    warnings: Vec<String>,
}

impl Athena {
    pub fn open(conn: &Connection, secrets: Secrets) -> Result<Athena> {
        Athena::open_with(conn, secrets, &|k| std::env::var(k).ok(), std::env::var_os("HOME").map(PathBuf::from))
    }

    /// `open` with the environment and the home directory given, so a test
    /// can hand in its own.
    pub fn open_with(conn: &Connection, secrets: Secrets, env: &dyn Fn(&str) -> Option<String>, home: Option<PathBuf>) -> Result<Athena> {
        let mut missing: Vec<String> = Vec::new();
        let profile = conn.profile.clone().or_else(|| env("AWS_PROFILE")).unwrap_or_else(|| "default".into());
        let shared = |name: &str, var: &str| -> Option<PathBuf> {
            env(var).map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(".aws").join(name)))
        };
        let region = conn.region.clone().or_else(|| env("AWS_REGION")).or_else(|| env("AWS_DEFAULT_REGION")).or_else(|| {
            shared("config", "AWS_CONFIG_FILE").and_then(|p| ini_value(&p, &[&format!("profile {profile}"), &profile], "region"))
        });
        if region.is_none() {
            missing.push("region (in datasources.yml, AWS_REGION, or the profile's config)".into());
        }
        // Half an explicit credential is a mistake to name, not a reason to
        // sign as whoever the next link in the chain is.
        match (&secrets.access_key_id, &secrets.secret_access_key) {
            (Some(_), None) => missing.push(format!("secret_access_key, beside the access_key_id in .zsql under {}", conn.uid)),
            (None, Some(_)) => missing.push(format!("access_key_id, beside the secret_access_key in .zsql under {}", conn.uid)),
            _ => {}
        }
        let creds = credentials(secrets, env, &profile, shared("credentials", "AWS_SHARED_CREDENTIALS_FILE"));
        if creds.is_none() && missing.iter().all(|m| !m.contains("beside the")) {
            missing.push(format!(
                "credentials: access_key_id and secret_access_key in .zsql under {uid} or in {a} and {s}, or AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, or the profile '{profile}' in ~/.aws/credentials",
                uid = conn.uid,
                a = crate::secret_env_var(&conn.uid, "access_key_id"),
                s = crate::secret_env_var(&conn.uid, "secret_access_key"),
            ));
        }
        if !missing.is_empty() {
            return Err(SourceError::NoCredentials(conn.uid.clone(), missing.join("; ")));
        }
        let region = region.expect("checked");
        let base = conn.url.clone().unwrap_or_else(|| format!("https://athena.{region}.amazonaws.com"));
        let note = http::plain(conn, &base, "athena")?;
        let host = base.split("://").nth(1).unwrap_or(&base).split('/').next().unwrap_or_default().to_string();
        Ok(Athena {
            agent: http::agent(conn)?,
            base: base.trim_end_matches('/').to_string(),
            host,
            conn: conn.clone(),
            region,
            creds: creds.expect("checked"),
            catalog: conn.catalog.clone().unwrap_or_else(|| "AwsDataCatalog".into()),
            database: conn.schema.clone().or_else(|| conn.database.clone()),
            warnings: note.into_iter().collect(),
        })
    }

    /// One action, signed, with the answer parsed or the refusal named.
    fn action(&self, target: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let body = body.to_string();
        let plain = vec![
            ("host".to_string(), self.host.clone()),
            ("content-type".to_string(), "application/x-amz-json-1.1".to_string()),
            ("x-amz-target".to_string(), format!("AmazonAthena.{target}")),
        ];
        let req = sigv4::Request { method: "POST", path: "/", query: "", headers: &plain, body: body.as_bytes() };
        let signed = sigv4::sign(&self.creds, &self.region, "athena", &req, &sigv4::now());
        let mut headers: Vec<(&str, &str)> = plain.iter().map(|(k, v)| (k.as_str(), v.as_str())).filter(|(k, _)| *k != "host").collect();
        headers.extend(signed.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        headers.push(("User-Agent", USER_AGENT));
        let (status, text) =
            http::call(&self.conn, &self.agent, http::Method::Post, &format!("{}/", self.base), &headers, Some(body.as_bytes()))?;
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if status != 200 {
            let kind = v.get("__type").and_then(|t| t.as_str()).unwrap_or("");
            return Err(SourceError::Query(
                self.conn.uid.clone(),
                format!("{status}: {kind} {}", http::reason(&text)).replace(":  ", ": "),
            ));
        }
        Ok(v)
    }

    fn send(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        let mut start = serde_json::json!({
            "QueryString": sql,
            "QueryExecutionContext": { "Catalog": self.catalog },
        });
        if let Some(db) = &self.database {
            start["QueryExecutionContext"]["Database"] = serde_json::Value::String(db.clone());
        }
        if let Some(out) = &self.conn.output_location {
            start["ResultConfiguration"] = serde_json::json!({ "OutputLocation": out });
        }
        if let Some(wg) = &self.conn.workgroup {
            start["WorkGroup"] = serde_json::Value::String(wg.clone());
        }
        let id = self
            .action("StartQueryExecution", start)?
            .get("QueryExecutionId")
            .and_then(|i| i.as_str())
            .map(str::to_string)
            .ok_or_else(|| SourceError::Query(self.conn.uid.clone(), "no QueryExecutionId in the answer".into()))?;

        // Queued, running, then settled; past the statement timeout the
        // query is stopped so it stops costing.
        let deadline = Instant::now() + self.conn.timeout();
        let mut wait = Duration::from_millis(500);
        loop {
            let v = self.action("GetQueryExecution", serde_json::json!({ "QueryExecutionId": id }))?;
            let state = v.pointer("/QueryExecution/Status/State").and_then(|s| s.as_str()).unwrap_or("");
            match state {
                "SUCCEEDED" => break,
                "FAILED" | "CANCELLED" => {
                    let why = v.pointer("/QueryExecution/Status/StateChangeReason").and_then(|s| s.as_str()).unwrap_or("");
                    return Err(SourceError::Query(self.conn.uid.clone(), format!("{}: {why}", state.to_lowercase()).trim().to_string()));
                }
                _ => {
                    if Instant::now() >= deadline {
                        let _ = self.action("StopQueryExecution", serde_json::json!({ "QueryExecutionId": id }));
                        return Err(SourceError::Query(
                            self.conn.uid.clone(),
                            format!("still running after {}s; stopped (query_timeout)", self.conn.timeout().as_secs()),
                        ));
                    }
                    std::thread::sleep(wait.min(deadline.saturating_duration_since(Instant::now())));
                    wait = (wait * 2).min(Duration::from_secs(5));
                }
            }
        }

        let mut columns: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        let mut token: Option<String> = None;
        let mut first = true;
        loop {
            let mut req = serde_json::json!({ "QueryExecutionId": id, "MaxResults": PAGE });
            if let Some(t) = &token {
                req["NextToken"] = serde_json::Value::String(t.clone());
            }
            let v = self.action("GetQueryResults", req)?;
            if columns.is_empty() {
                columns = v
                    .pointer("/ResultSet/ResultSetMetadata/ColumnInfo")
                    .and_then(|c| c.as_array())
                    .map(|c| c.iter().filter_map(|i| i.get("Name").and_then(|n| n.as_str()).map(str::to_string)).collect())
                    .unwrap_or_default();
            }
            let page = v.pointer("/ResultSet/Rows").and_then(|r| r.as_array()).cloned().unwrap_or_default();
            for (i, row) in page.iter().enumerate() {
                let cells: Vec<Option<String>> = row
                    .get("Data")
                    .and_then(|d| d.as_array())
                    .map(|d| d.iter().map(|c| c.get("VarCharValue").and_then(|x| x.as_str()).map(str::to_string)).collect())
                    .unwrap_or_default();
                // The header row, repeated as data on a SELECT's first page.
                if first
                    && i == 0
                    && !columns.is_empty()
                    && cells.iter().map(|c| c.as_deref().unwrap_or_default()).eq(columns.iter().map(String::as_str))
                {
                    continue;
                }
                rows.push(cells);
                if rows.len() > max_rows {
                    return Err(SourceError::TooManyRows(self.conn.uid.clone(), max_rows));
                }
            }
            first = false;
            token = v.get("NextToken").and_then(|t| t.as_str()).map(str::to_string);
            if token.is_none() {
                break;
            }
        }
        Ok(Rows { columns, rows })
    }
}

/// The first three links of the credential chain, in their order.
fn credentials(
    secrets: Secrets,
    env: &dyn Fn(&str) -> Option<String>,
    profile: &str,
    shared_file: Option<PathBuf>,
) -> Option<sigv4::Credentials> {
    if let (Some(id), Some(key)) = (&secrets.access_key_id, &secrets.secret_access_key) {
        return Some(sigv4::Credentials {
            access_key_id: id.expose().to_string(),
            secret_access_key: key.clone(),
            session_token: secrets.session_token,
        });
    }
    if let (Some(id), Some(key)) = (env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY")) {
        return Some(sigv4::Credentials {
            access_key_id: id,
            secret_access_key: Secret(key),
            session_token: env("AWS_SESSION_TOKEN").map(Secret),
        });
    }
    let file = shared_file?;
    let id = ini_value(&file, &[profile], "aws_access_key_id")?;
    let key = ini_value(&file, &[profile], "aws_secret_access_key")?;
    Some(sigv4::Credentials {
        access_key_id: id,
        secret_access_key: Secret(key),
        session_token: ini_value(&file, &[profile], "aws_session_token").map(Secret),
    })
}

/// One key of one section of an INI file, the first of `sections` that
/// holds it. The AWS files are plain enough for this: `[name]` lines,
/// `key = value` lines, `#` and `;` comments.
fn ini_value(path: &std::path::Path, sections: &[&str], key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for want in sections {
        let mut inside = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                inside = name.trim() == *want;
                continue;
            }
            if inside {
                if let Some((k, v)) = line.split_once('=') {
                    if k.trim().eq_ignore_ascii_case(key) {
                        let v = v.trim();
                        if !v.is_empty() {
                            return Some(v.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

impl Source for Athena {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        read_only(sql)?;
        self.send(sql, max_rows)
    }

    fn adapter(&self) -> &str {
        "athena"
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

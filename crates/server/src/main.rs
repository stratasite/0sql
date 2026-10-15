//! zsqld: the server, configured from the environment. The same server
//! is `zsql serve`, configured from flags; this binary is for the
//! deployment that reads `/etc/zsqld.env`, and it is the one place a
//! `ZSQL_*` variable is read.

use std::path::PathBuf;
use std::time::Duration;
use zsql_server::limits::{AdmissionCounts, Mode, Rate};
use zsql_server::{Caps, Config, MailConfig, McpSettings};

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn on(name: &str) -> bool {
    matches!(var(name).as_deref(), Some("on" | "1" | "true"))
}

fn rate(name: &str, slot: &mut Rate) -> Result<(), String> {
    if let Some(v) = var(name) {
        *slot = Rate::parse(&v).map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
}

fn count(name: &str, slot: &mut usize) -> Result<(), String> {
    if let Some(v) = var(name) {
        *slot = v.parse().map_err(|_| format!("{name}: {v:?} is not a number"))?;
    }
    Ok(())
}

/// A branch cap: a number, or `off`.
fn cap(name: &str, slot: &mut usize) -> Result<(), String> {
    match var(name) {
        Some(v) if v.eq_ignore_ascii_case("off") => *slot = usize::MAX,
        Some(v) => *slot = v.parse().map_err(|_| format!("{name}: {v:?} is not a number"))?,
        None => {}
    }
    Ok(())
}

/// A file named by a variable, read whole, or nothing.
fn file(name: &str) -> Result<Option<String>, String> {
    match var(name) {
        Some(path) => {
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{name}: cannot read {path}: {e}"))?;
            eprintln!("zsqld: mcp: {name} read from {path}");
            Ok(Some(text))
        }
        None => Ok(None),
    }
}

fn from_env() -> Result<Config, String> {
    let bind = var("ZSQL_BIND").unwrap_or_else(|| "127.0.0.1:3699".into());
    let accounts = on("ZSQL_ACCOUNTS");
    // The limits are about sharing one server between tenants, so a
    // single-tenant server starts with them off.
    let mut limits = zsql_server::limits::Config { mode: if accounts { Mode::On } else { Mode::Off }, ..Default::default() };
    if let Some(v) = var("ZSQL_LIMITS") {
        limits.mode = Mode::parse(&v);
    }
    rate("ZSQL_LIMIT_PLAN", &mut limits.plan)?;
    rate("ZSQL_LIMIT_EXPLORE", &mut limits.explore)?;
    rate("ZSQL_LIMIT_HEAVY", &mut limits.heavy)?;
    rate("ZSQL_LIMIT_MANAGE", &mut limits.manage)?;
    rate("ZSQL_LIMIT_KEY_PLAN", &mut limits.key_plan)?;
    rate("ZSQL_LIMIT_KEY_EXPLORE", &mut limits.key_explore)?;
    rate("ZSQL_LIMIT_LOGIN", &mut limits.login)?;
    rate("ZSQL_LIMIT_SIGNUP", &mut limits.signup)?;
    rate("ZSQL_LIMIT_INVITE", &mut limits.invite)?;
    rate("ZSQL_LIMIT_RESET", &mut limits.reset)?;
    rate("ZSQL_LIMIT_BAD_KEY", &mut limits.bad_key)?;
    rate("ZSQL_LIMIT_HANDSHAKE", &mut limits.handshake)?;
    if let Some(v) = var("ZSQL_LIMIT_EMAIL_FAILURES") {
        limits.email_failures = if v.eq_ignore_ascii_case("off") {
            0
        } else {
            v.parse().map_err(|_| format!("ZSQL_LIMIT_EMAIL_FAILURES: {v:?} is not a number"))?
        };
    }
    if let Some(v) = var("ZSQL_LIMIT_EMAIL_WINDOW") {
        limits.email_window =
            Duration::from_secs(v.parse().map_err(|_| format!("ZSQL_LIMIT_EMAIL_WINDOW: {v:?} is not a number of seconds"))?);
    }
    limits.trusted_proxy = on("ZSQL_TRUSTED_PROXY");

    let mut admission = AdmissionCounts::default();
    count("ZSQL_ADMIT_PLAN", &mut admission.plan)?;
    count("ZSQL_ADMIT_EXPLORE", &mut admission.explore)?;
    count("ZSQL_ADMIT_HEAVY", &mut admission.heavy)?;
    count("ZSQL_ADMIT_MANAGE", &mut admission.manage)?;
    count("ZSQL_ADMIT_PASSWORD", &mut admission.password)?;

    let mut caps = Caps::default();
    cap("ZSQL_MAX_BRANCHES_PER_PROJECT", &mut caps.per_project)?;
    cap("ZSQL_MAX_BRANCHES_PER_ACCOUNT", &mut caps.per_account)?;

    let mut mail =
        MailConfig { command: var("ZSQL_MAIL_COMMAND"), from: var("ZSQL_MAIL_FROM").unwrap_or_default(), ..MailConfig::default() };
    if let Some(v) = var("ZSQL_MAIL_TIMEOUT") {
        mail.timeout = Duration::from_secs(v.parse().map_err(|_| format!("ZSQL_MAIL_TIMEOUT: {v:?} is not a number of seconds"))?);
    }

    let tools = var("ZSQL_MCP_TOOLS").map(|v| v.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect());
    let mcp = McpSettings::new(file("ZSQL_MCP_PROMPTS")?.as_deref(), file("ZSQL_MCP_SPEC_SCHEMA")?.as_deref(), tools)?;

    Ok(Config {
        data_dir: PathBuf::from(var("ZSQL_DATA_DIR").unwrap_or_else(|| "data".into())),
        bind,
        accounts,
        signup_open: !matches!(var("ZSQL_SIGNUP").as_deref(), Some("closed" | "off" | "0")),
        public_url: var("ZSQL_PUBLIC_URL"),
        limits,
        admission,
        caps,
        mail,
        mcp,
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = from_env().map_err(|e| anyhow::anyhow!("zsqld: {e}"))?;
    zsql_server::run(config).await
}

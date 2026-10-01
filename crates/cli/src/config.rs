//! Where the CLI finds the project, the server and the API key.
//!
//! - `project.yml` in the project directory: `uid`, `name`, `production_branch`,
//!   and optionally `server` (a self-hosted zsqld).
//! - `.zsql` in the project directory (or `.strata`, the strata CLI's file,
//!   when `.zsql` is absent): `api_key`, `server`, and per-datasource secrets.
//! - `~/.zsql/config`: the same keys, account-wide.
//! - `ZSQL_SERVER` and `ZSQL_API_KEY` in the environment, which win.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub const DEFAULT_SERVER: &str = "http://127.0.0.1:3699";
pub const LOCAL_FILE: &str = ".zsql";
pub const STRATA_FILE: &str = ".strata";

#[derive(Debug, Clone, Default)]
pub struct Project {
    pub dir: PathBuf,
    pub uid: String,
    pub name: String,
    pub production_branch: String,
    /// `server:` from project.yml, when set.
    pub server: Option<String>,
}

/// Walks up from `start` to the directory holding project.yml.
pub fn find_project(start: &Path) -> Result<Project> {
    let mut dir = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    loop {
        let file = dir.join("project.yml");
        if file.exists() {
            return read_project(&dir);
        }
        if !dir.pop() {
            anyhow::bail!("no project.yml here or above; run `zsql init` to start a project");
        }
    }
}

pub fn read_project(dir: &Path) -> Result<Project> {
    let file = dir.join("project.yml");
    let text = std::fs::read_to_string(&file).with_context(|| file.display().to_string())?;
    let v: serde_yaml::Value = serde_yaml::from_str(&text).with_context(|| file.display().to_string())?;
    let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("").trim().to_string();
    let uid = v.get("uid").and_then(|u| u.as_str()).map(|u| u.trim().to_lowercase()).filter(|u| !u.is_empty()).unwrap_or_else(|| slug(&name));
    if uid.is_empty() {
        anyhow::bail!("{}: name or uid is required", file.display());
    }
    Ok(Project {
        dir: dir.to_path_buf(),
        uid,
        name,
        production_branch: v.get("production_branch").and_then(|b| b.as_str()).unwrap_or("main").trim().to_string(),
        server: v.get("server").and_then(|s| s.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
    })
}

/// ActiveSupport `parameterize`, as the server derives uids.
pub fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut dash = false;
    for ch in s.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
            dash = ch == '-';
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// A key from the local secrets file (`.zsql`, else `.strata`) or the
/// account-wide one.
pub fn secret(project_dir: Option<&Path>, key: &str) -> Option<String> {
    let mut files: Vec<PathBuf> = Vec::new();
    if let Some(dir) = project_dir {
        let local = dir.join(LOCAL_FILE);
        files.push(if local.exists() { local } else { dir.join(STRATA_FILE) });
    }
    if let Some(home) = std::env::var_os("HOME") {
        files.push(PathBuf::from(home).join(".zsql").join("config"));
    }
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(&text) else { continue };
        if let Some(s) = v.get(key).and_then(|s| s.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            return Some(s);
        }
    }
    None
}

/// The server to talk to: flag, `ZSQL_SERVER`, the secrets files, project.yml, default.
pub fn server(flag: Option<&str>, project: Option<&Project>) -> String {
    flag.map(str::to_string)
        .or_else(|| std::env::var("ZSQL_SERVER").ok().filter(|s| !s.is_empty()))
        .or_else(|| secret(project.map(|p| p.dir.as_path()), "server"))
        .or_else(|| project.and_then(|p| p.server.clone()))
        .unwrap_or_else(|| DEFAULT_SERVER.to_string())
        .trim_end_matches('/')
        .to_string()
}

pub fn api_key(project: Option<&Project>) -> Option<String> {
    std::env::var("ZSQL_API_KEY").ok().filter(|s| !s.is_empty()).or_else(|| secret(project.map(|p| p.dir.as_path()), "api_key"))
}

/// Writes `key: value` into the local secrets file, replacing an existing line.
pub fn write_secret(project_dir: &Path, key: &str, value: &str) -> Result<()> {
    let path = project_dir.join(LOCAL_FILE);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().filter(|l| !l.trim_start().starts_with(&format!("{key}:"))).map(str::to_string).collect();
    if lines.is_empty() {
        lines.push("# zsql local configuration: api key, server, datasource secrets. Do not commit.".into());
    }
    lines.push(format!("{key}: {value}"));
    std::fs::write(&path, lines.join("\n") + "\n").with_context(|| path.display().to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// The checked-out git branch of the project, which is what deploys go to.
pub fn git_branch(dir: &Path) -> Result<String> {
    let out = std::process::Command::new("git").args(["rev-parse", "--abbrev-ref", "HEAD"]).current_dir(dir).output().context("running git; is it installed?")?;
    if !out.status.success() {
        anyhow::bail!("{} is not a git repository; zsql deploys the checked-out branch, so run `git init` and commit first", dir.display());
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if branch == "HEAD" {
        anyhow::bail!("detached HEAD; check out a branch to deploy");
    }
    Ok(branch)
}

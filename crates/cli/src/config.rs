//! Where the CLI finds the project, the server and the API key.
//!
//! - `project.yml` in the project directory: `uid`, `name`, `production_branch`,
//!   and optionally `server` (a self-hosted server).
//! - `.zsql` in the project directory (or `.strata`, the strata CLI's file,
//!   when `.zsql` is absent): `api_key`, `server`, and per-datasource secrets.
//! - `~/.zsql/config`: the same keys, account-wide.
//! - `ZSQL_SERVER` and `ZSQL_API_KEY` in the environment, which win.
//!
//! `~/.zsql` is also where the repl history lives. `ZSQL_HOME` moves
//! the whole directory, which is how tests keep out of the real one.

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
            anyhow::bail!("no project.yml here or above; run `zsql init` to start a project, or add -r --uid UID to use a server's copy");
        }
    }
}

pub fn read_project(dir: &Path) -> Result<Project> {
    let file = dir.join("project.yml");
    let text = std::fs::read_to_string(&file).with_context(|| file.display().to_string())?;
    let v: serde_yaml::Value = serde_yaml::from_str(&text).with_context(|| file.display().to_string())?;
    let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("").trim().to_string();
    let uid = v
        .get("uid")
        .and_then(|u| u.as_str())
        .map(|u| u.trim().to_lowercase())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| zsql_project::parameterize(&name));
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

/// The account-wide directory: `ZSQL_HOME`, else `~/.zsql`.
pub fn home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("ZSQL_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(h));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".zsql"))
}

/// A key from the local secrets file (`.zsql`, else `.strata`) or the
/// account-wide one.
pub fn secret(project_dir: Option<&Path>, key: &str) -> Option<String> {
    let mut files: Vec<PathBuf> = Vec::new();
    if let Some(dir) = project_dir {
        let local = dir.join(LOCAL_FILE);
        files.push(if local.exists() { local } else { dir.join(STRATA_FILE) });
    }
    if let Some(home) = home() {
        files.push(home.join("config"));
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
    let git = |args: &[&str]| -> Result<(bool, String)> {
        let out = std::process::Command::new("git").args(args).current_dir(dir).output().context("running git; is it installed?")?;
        Ok((out.status.success(), String::from_utf8_lossy(&out.stdout).trim().to_string()))
    };
    if !git(&["rev-parse", "--git-dir"])?.0 {
        anyhow::bail!("{} is not a git repository; zsql deploys the checked-out branch, so run `git init` and commit first", dir.display());
    }
    // An unborn HEAD is a repository all the same: it names its branch
    // symbolically, it just has no commit for `--abbrev-ref` to resolve.
    if !git(&["rev-parse", "--verify", "--quiet", "HEAD"])?.0 {
        let unborn = git(&["symbolic-ref", "--short", "--quiet", "HEAD"])?.1;
        let on = if unborn.is_empty() { String::new() } else { format!(" on `{unborn}`") };
        anyhow::bail!("{} is a git repository with no commits; zsql deploys the checked-out branch, so commit{on} first", dir.display());
    }
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"])?.1;
    if branch == "HEAD" {
        anyhow::bail!("detached HEAD; check out a branch to deploy");
    }
    Ok(branch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// The four HEAD states are four different complaints. A project inside
    /// a fresh `git init` used to be told it was not a repository and to run
    /// `git init`, the one step that could not have helped.
    #[test]
    fn git_branch_tells_no_repository_from_no_commits() {
        let root = std::env::temp_dir().join(format!("zsql-git-branch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        let project = repo.join("semantic");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(root.join("plain")).unwrap();

        let err = git_branch(&root.join("plain")).unwrap_err().to_string();
        assert!(err.contains("is not a git repository"), "{err}");

        run_git(&repo, &["init", "-q", "-b", "main", "."]);
        let err = git_branch(&project).unwrap_err().to_string();
        assert!(err.contains("is a git repository with no commits"), "{err}");
        assert!(err.contains("commit on `main` first"), "{err}");

        std::fs::write(repo.join("f"), "x").unwrap();
        run_git(&repo, &["add", "f"]);
        run_git(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "c"]);
        assert_eq!(git_branch(&project).unwrap(), "main");

        run_git(&repo, &["checkout", "-q", "--detach", "HEAD"]);
        let err = git_branch(&project).unwrap_err().to_string();
        assert!(err.contains("detached HEAD"), "{err}");

        let _ = std::fs::remove_dir_all(&root);
    }
}

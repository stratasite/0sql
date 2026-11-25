//! Where the CLI finds the project and the server.
//!
//! - `project.yml` in the project directory: `uid`, `name`, `production_branch`,
//!   and optionally `org` (the org it is deployed under; the server's
//!   default org without it) and `server` (a self-hosted server).
//! - `.zsql` in the project directory (or `.strata`, the strata CLI's file,
//!   when `.zsql` is absent): `server`, and per-datasource secrets. An
//!   `api_key` left there from when a server took a deploy token is not read.
//! - `~/.zsql/config`: the same keys, for every project of this user.
//! - `ZSQL_SERVER` in the environment, which wins.
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
    /// `org:` from project.yml: the org the project is deployed under
    /// and asked for in. Without it, the server's default org.
    pub org: Option<String>,
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
        org: v.get("org").and_then(|o| o.as_str()).map(|o| o.trim().to_lowercase()).filter(|o| !o.is_empty()),
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

/// Writes `key: value` into the local secrets file, replacing an existing line.
pub fn write_secret(project_dir: &Path, key: &str, value: &str) -> Result<()> {
    let path = project_dir.join(LOCAL_FILE);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines = without_key(&existing, key);
    if lines.is_empty() {
        lines.push("# zsql local configuration: server, datasource secrets. Do not commit.".into());
    }
    lines.push(format!("{key}: {}", scalar(value)));
    std::fs::write(&path, lines.join("\n") + "\n").with_context(|| path.display().to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// The file's lines without a top-level key: its own line and the more
/// indented or blank ones under it, which are the rest of a value
/// somebody wrote by hand over several lines, as a PEM often is.
fn without_key(text: &str, key: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut dropping = false;
    for line in text.lines() {
        if line.starts_with(&format!("{key}:")) {
            dropping = true;
            continue;
        }
        if dropping && (line.starts_with([' ', '\t']) || line.trim().is_empty()) {
            continue;
        }
        dropping = false;
        out.push(line.to_string());
    }
    out
}

/// A value as one line of YAML that reads back as exactly that text:
/// as it is where that is so, and otherwise double-quoted with its
/// escapes, so a PEM's line breaks, a `#`, a `: ` or a password that is
/// all digits survive the file.
fn scalar(value: &str) -> String {
    let plain = !value.contains(['\n', '\r'])
        && serde_yaml::from_str::<serde_yaml::Value>(&format!("k: {value}"))
            .is_ok_and(|v| v.get("k").and_then(|k| k.as_str()) == Some(value) && v.as_mapping().is_some_and(|m| m.len() == 1));
    if plain {
        value.to_string()
    } else {
        // JSON's string is YAML's double-quoted scalar.
        serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}"))
    }
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

    #[test]
    fn a_key_written_over_several_lines_is_replaced_whole() {
        let text = "server: s\nwh_private_key: |\n  -----BEGIN-----\n\n  abc\nwh_password: pw\nwh:\n  password: nested\n";
        assert_eq!(without_key(text, "wh_private_key"), ["server: s", "wh_password: pw", "wh:", "  password: nested"]);
        assert_eq!(without_key(text, "wh_password").len(), 7, "one line, and the nested entry of that name is another's");
    }

    #[test]
    fn a_secret_is_written_on_one_line_and_reads_back_as_itself() {
        assert_eq!(scalar("hunter2"), "hunter2");
        assert_eq!(scalar("zsql@p.iam.gserviceaccount.com"), "zsql@p.iam.gserviceaccount.com");
        for value in
            ["-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----", "123456", "a #b", "k: v", "null", "~", " padded", "é\t\\"]
        {
            let line = format!("k: {}", scalar(value));
            assert!(!line.contains('\n'), "{line}");
            let read: serde_yaml::Value = serde_yaml::from_str(&line).unwrap();
            assert_eq!(read["k"].as_str(), Some(value), "{line}");
        }
    }

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

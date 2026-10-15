//! The project directory as files: what to send in a deploy, and how
//! to tell when it has changed.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A file as last seen: its size and modification time, or that it
/// was not there.
pub type Stamp = (PathBuf, Option<(u64, SystemTime)>);

/// The given paths as they are now.
pub fn stamps(paths: &[PathBuf]) -> Vec<Stamp> {
    paths.iter().map(|p| (p.clone(), std::fs::metadata(p).ok().and_then(|m| Some((m.len(), m.modified().ok()?))))).collect()
}

/// Every model, test and values file under the directory as it is
/// now, each with its size and modification time, so a file edited,
/// added or removed is a change. Dot directories and `target` are not
/// looked into, and a directory reached through a link is not followed,
/// so a link back up the tree cannot recurse.
pub fn stamps_under(dir: &Path) -> Vec<Stamp> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || name == "target" {
                continue;
            }
            let Ok(kind) = e.file_type() else { continue };
            if kind.is_dir() {
                stack.push(p);
            } else if kind.is_file() && matches!(p.extension().and_then(|x| x.to_str()), Some("yml" | "yaml" | "gz")) {
                files.push(p);
            }
        }
    }
    files.sort();
    stamps(&files)
}

/// Calls `on_change` whenever the project's files move, until `done`
/// says so. The files are looked at twice a second; a change is given
/// a moment to finish being written before it is acted on.
pub fn watch(dir: &Path, done: impl Fn() -> bool, mut on_change: impl FnMut()) {
    let mut last = stamps_under(dir);
    while !done() {
        std::thread::sleep(Duration::from_millis(500));
        let now = stamps_under(dir);
        if now != last {
            std::thread::sleep(Duration::from_millis(200));
            on_change();
            last = stamps_under(dir);
        }
    }
}

/// The keys a datasource entry may carry that are a secret, at any
/// depth: a nested `oauth: {client_secret: ...}` is as much one as a
/// top-level `password`.
const SECRET_KEYS: [&str; 12] = [
    "password",
    "secret",
    "private_key",
    "personal_access_token",
    "secret_access_key",
    "access_key_id",
    "oauth_client_secret",
    "oauth_client_id",
    "client_secret",
    "client_id",
    "api_key",
    "token",
];

fn strip_secrets(v: &mut serde_yaml::Value) {
    if let Some(map) = v.as_mapping_mut() {
        map.retain(|k, _| !k.as_str().is_some_and(|k| SECRET_KEYS.contains(&k)));
        for (_, inner) in map.iter_mut() {
            strip_secrets(inner);
        }
    } else if let Some(items) = v.as_sequence_mut() {
        for item in items {
            strip_secrets(item);
        }
    }
}

/// The project as a tar.gz: YAML models, tests, security, datasources.yml
/// with its secrets removed, and samples/values.jsonl.gz. Never .zsql,
/// .strata, .git or data files.
pub fn archive(project: &Path) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        let mut add = |rel: &str, bytes: &[u8]| -> Result<()> {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, rel, bytes)?;
            Ok(())
        };
        for name in ["project.yml", "security.yml"] {
            if let Ok(bytes) = std::fs::read(project.join(name)) {
                add(name, &bytes)?;
            }
        }
        if let Ok(text) = std::fs::read_to_string(project.join("datasources.yml")) {
            let mut v: serde_yaml::Value = serde_yaml::from_str(&text)?;
            strip_secrets(&mut v);
            add("datasources.yml", serde_yaml::to_string(&v)?.as_bytes())?;
        }
        if let Ok(bytes) = std::fs::read(project.join("samples").join("values.jsonl.gz")) {
            add("samples/values.jsonl.gz", &bytes)?;
        }
        for dir in ["models", "tests"] {
            let root = project.join(dir);
            if !root.is_dir() {
                continue;
            }
            let mut files = Vec::new();
            walk_yaml(&root, &mut files)?;
            files.sort();
            for f in files {
                let rel = f.strip_prefix(project)?.to_string_lossy().replace('\\', "/");
                add(&rel, &std::fs::read(&f)?)?;
            }
        }
        tar.into_inner()?.finish()?;
    }
    Ok(buf)
}

/// Every YAML file under `dir`. A directory reached through a link is
/// not followed.
fn walk_yaml(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            walk_yaml(&path, out)?;
        } else if kind.is_file() && matches!(path.extension().and_then(|e| e.to_str()), Some("yml" | "yaml")) {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A secret under any key, at any depth, stays home.
    #[test]
    fn secrets_are_stripped_at_every_depth() {
        let text = "wh:\n  adapter: postgres\n  password: p\n  oauth:\n    client_secret: s\n    scope: read\n  hosts:\n    - name: a\n      token: t\n";
        let mut v: serde_yaml::Value = serde_yaml::from_str(text).unwrap();
        strip_secrets(&mut v);
        let out = serde_yaml::to_string(&v).unwrap();
        assert!(!out.contains("password") && !out.contains("client_secret") && !out.contains("token"), "{out}");
        assert!(out.contains("adapter: postgres") && out.contains("scope: read") && out.contains("name: a"), "{out}");
    }
}

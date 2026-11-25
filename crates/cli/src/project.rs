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

/// Each datasource's entry with its secrets in it: the ones written
/// there, and over them the ones this machine holds for it in `.zsql`,
/// the environment or the home config, which are what a query run here
/// connects with. A deploy carries them, and on the server they are the
/// datasource's credential for that branch, kept apart from the model.
/// `.zsql` itself is never sent: it holds more than one project's.
fn with_secrets(project: &Path, v: &mut serde_yaml::Value) -> Vec<String> {
    let mut carried = Vec::new();
    let Some(entries) = v.as_mapping_mut() else { return carried };
    for (uid, entry) in entries.iter_mut() {
        let (Some(uid), Some(entry)) = (uid.as_str(), entry.as_mapping_mut()) else { continue };
        let held = zsql_datasource::secrets(Some(project), uid);
        for (field, secret) in held.fields() {
            entry.insert(field.into(), secret.into());
        }
        if entry.keys().any(|k| k.as_str().is_some_and(zsql_core::is_secret)) {
            carried.push(uid.to_string());
        }
    }
    carried
}

/// The datasources a deploy of this project carries a secret for.
pub fn carries_secrets_for(project: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(project.join("datasources.yml")) else { return Vec::new() };
    let Ok(mut v) = serde_yaml::from_str::<serde_yaml::Value>(&text) else { return Vec::new() };
    with_secrets(project, &mut v)
}

/// The project as a tar.gz: YAML models, tests, security, datasources.yml
/// with each datasource's secrets in its entry, and
/// samples/values.jsonl.gz. Never .zsql, .strata, .git or data files.
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
            with_secrets(project, &mut v);
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

    /// A deploy carries each datasource's secrets: the ones in its
    /// entry and, over them, the ones `.zsql` holds for it.
    #[test]
    fn a_datasources_secrets_go_with_it() {
        let dir = std::env::temp_dir().join(format!("zsql-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("project.yml"), "name: P\nuid: p\n").unwrap();
        std::fs::write(
            dir.join("datasources.yml"),
            "wh:\n  adapter: postgres\n  host: db\n  password: written-here\nlake:\n  adapter: duckdb\n  file: lake.db\n",
        )
        .unwrap();
        std::fs::write(dir.join(".zsql"), "wh:\n  password: from-dot-zsql\n  oauth_token: tok\n").unwrap();
        let mut v: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(dir.join("datasources.yml")).unwrap()).unwrap();
        assert_eq!(with_secrets(&dir, &mut v), ["wh"]);
        assert_eq!((v["wh"]["password"].as_str(), v["wh"]["oauth_token"].as_str()), (Some("from-dot-zsql"), Some("tok")));
        assert_eq!((v["wh"]["host"].as_str(), v["lake"].get("password")), (Some("db"), None));
        assert_eq!(carries_secrets_for(&dir), ["wh"]);
        // And the archive holds them, and not the file they came from.
        let bytes = archive(&dir).unwrap();
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]));
        let mut names = Vec::new();
        for entry in tar.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().display().to_string();
            if name == "datasources.yml" {
                let mut text = String::new();
                std::io::Read::read_to_string(&mut entry, &mut text).unwrap();
                assert!(text.contains("from-dot-zsql") && !text.contains("written-here"), "{text}");
            }
            names.push(name);
        }
        assert_eq!(names, ["project.yml", "datasources.yml"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

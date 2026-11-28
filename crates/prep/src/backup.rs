//! The backup: with an object store, each dataset's files are copied
//! under `datasets/<org>/<project>/<id>/` there, and each source's under
//! `datasets/<org>/<project>/sources/<id>/`, read back only by a restore.
//!
//! A copy sends what is new or changed since it last sent it (a file
//! is known by its size and modified time; most never change, and a
//! version's manifest does when a source it pointed at is deleted and
//! its rows copied in), and the manifest last: one in the store
//! is whole once its manifest is there, and a restore takes only those.
//! A copy that fails is tried again, and then the dataset or source
//! says it is not backed up; the job it followed has still succeeded. A
//! delete removes the copy too.

use crate::error::{Error, Result};
use crate::store;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use zsql_exec::Objects;

/// Where datasets go in the store.
pub const PREFIX: &str = "datasets";

/// The files sent already, each with its size and modified time, kept
/// beside them, so a copy sends only what is new or changed.
const SENT: &str = ".backed-up.json";

/// The files that change: sent every time.
const CHANGING: [&str; 2] = [store::MANIFEST, store::SOURCE];

/// Where sources are, below a project, in the store.
const SOURCES: &str = "sources";

/// How many times a copy is tried.
const TRIES: usize = 3;

/// Where a dataset's backup stands, as its manifest says.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Backup {
    pub at: String,
    /// Why it is not backed up, when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<String>,
}

fn objects_error(e: zsql_exec::Error) -> Error {
    Error::Internal(format!("the object store: {e}"))
}

fn tried<T>(mut work: impl FnMut() -> std::result::Result<T, zsql_exec::Error>) -> Result<T> {
    let mut last = None;
    for i in 0..TRIES {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(200 * i as u64));
        }
        match work() {
            Ok(v) => return Ok(v),
            Err(e) => last = Some(e),
        }
    }
    Err(objects_error(last.expect("tried at least once")))
}

/// What a directory is a copy of, in the store: its org, project and
/// the key below the project (`<id>`, or `sources/<id>`), and its
/// manifest's name.
pub(crate) fn names_of(dir: &Path) -> Option<(String, String, Vec<String>, &'static str)> {
    let id = dir.file_name()?.to_string_lossy().to_string();
    let parent = dir.parent()?;
    let (below, datasets, manifest) = if parent.file_name()? == store::SOURCES {
        (vec![SOURCES.to_string(), id], parent.parent()?, store::SOURCE)
    } else {
        (vec![id], parent, store::MANIFEST)
    };
    let project_dir = datasets.parent()?;
    let project = project_dir.file_name()?.to_string_lossy().to_string();
    let org = project_dir.parent()?.file_name()?.to_string_lossy().to_string();
    Some((org, project, below, manifest))
}

/// Every file of a dataset's directory, by its path below it, the
/// dot files of jobs in flight left out.
fn files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            // A job's marker and an interruption are this node's, of
            // what ran here; a restored dataset starts without them.
            let here = d == dir && (name == crate::recovery::MARKER || name == crate::recovery::INTERRUPTED);
            if name.starts_with('.') || name.contains(".tmp-") || here {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(rel) = p.strip_prefix(dir) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    out
}

/// A file as a copy knows it: its size and when it was last written.
fn stamp(file: &Path) -> String {
    let Ok(m) = std::fs::metadata(file) else { return String::new() };
    let at = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{}:{at}", m.len())
}

fn key(org: &str, project: &str, below: &[String], rel: &str) -> Vec<String> {
    let mut k = vec![PREFIX.to_string(), org.to_string(), project.to_string()];
    k.extend(below.iter().cloned());
    k.extend(rel.split('/').map(str::to_string));
    k
}

fn put(objects: &Objects, path: &[String], file: &Path) -> Result<()> {
    let bytes = std::fs::read(file).map_err(|e| Error::io(file, e))?;
    let parts: Vec<&str> = path.iter().map(String::as_str).collect();
    tried(|| objects.put(&parts, bytes.clone())).map(|_| ())
}

/// Copies what a dataset or a source has not sent yet, the manifest
/// last.
pub(crate) fn copy(objects: &Objects, dir: &Path) -> Result<()> {
    let (org, project, below, manifest) =
        names_of(dir).ok_or_else(|| Error::Internal(format!("{} is not a dataset's or a source's directory", dir.display())))?;
    let sent_path = dir.join(SENT);
    let mut sent: BTreeMap<String, String> = store::read_json(&sent_path).unwrap_or_default();
    let here = files(dir);
    sent.retain(|rel, _| here.contains(rel));
    for rel in here {
        let stamp = stamp(&dir.join(&rel));
        if rel == manifest || (sent.get(&rel) == Some(&stamp) && !CHANGING.contains(&rel.as_str())) {
            continue;
        }
        put(objects, &key(&org, &project, &below, &rel), &dir.join(&rel))?;
        if !CHANGING.contains(&rel.as_str()) {
            sent.insert(rel, stamp);
        }
    }
    store::write_json(&sent_path, &sent)?;
    put(objects, &key(&org, &project, &below, manifest), &dir.join(manifest))
}

/// Removes a dataset's copy, or a source's (`sources/<id>`).
pub(crate) fn forget(objects: &Objects, org: &str, project: &str, below: &[&str]) -> Result<()> {
    let mut path = vec![PREFIX, org, project];
    path.extend(below);
    tried(|| objects.delete_under(&path)).map(|_| ())
}

/// Brings back every dataset and source of an org the store has whole
/// and this node does not, the sources first; the ones brought back, as
/// `project/id`.
pub(crate) fn restore(objects: &Objects, data_dir: &Path, org: &str) -> Result<Vec<String>> {
    let keys = tried(|| objects.list(&[PREFIX, org]))?;
    // (project, is a dataset, the key below the project) to the files.
    let mut by_item: std::collections::BTreeMap<(String, bool, Vec<String>), Vec<Vec<String>>> = std::collections::BTreeMap::new();
    for k in keys {
        match k.as_slice() {
            [project, s, id, rest @ ..] if s == SOURCES && !rest.is_empty() => {
                by_item.entry((project.clone(), false, vec![s.clone(), id.clone()])).or_default().push(rest.to_vec());
            }
            [project, id, rest @ ..] if !rest.is_empty() && id != SOURCES => {
                by_item.entry((project.clone(), true, vec![id.clone()])).or_default().push(rest.to_vec());
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for ((project, is_dataset, below), rels) in by_item {
        let id = below.last().cloned().unwrap_or_default();
        let (known, manifest) = if is_dataset { (crate::is_id(&id), store::MANIFEST) } else { (crate::is_source_id(&id), store::SOURCE) };
        if !known || !rels.iter().any(|r| r.len() == 1 && r[0] == manifest) {
            continue;
        }
        let root = data_dir.join(org).join(&project).join(store::DIR);
        let home = if is_dataset { root.clone() } else { root.join(store::SOURCES) };
        let target = home.join(&id);
        if target.join(manifest).exists() {
            continue;
        }
        let staging = home.join(format!(".restore-{id}-{}", crate::time::nanos()));
        let brought = (|| -> Result<()> {
            for rel in &rels {
                if rel.iter().any(|p| p == ".." || p.is_empty() || p.starts_with('.')) {
                    continue;
                }
                let mut path: Vec<&str> = vec![PREFIX, org, &project];
                path.extend(below.iter().map(String::as_str));
                path.extend(rel.iter().map(String::as_str));
                let bytes = tried(|| objects.get(&path))?;
                store::write_file(&staging.join(rel.join("/")), &bytes)?;
            }
            // What was sent is what is here: a later copy sends nothing again.
            let sent: BTreeMap<String, String> = rels
                .iter()
                .map(|r| r.join("/"))
                .filter(|r| !CHANGING.contains(&r.as_str()))
                .map(|r| {
                    let at = stamp(&staging.join(&r));
                    (r, at)
                })
                .collect();
            store::write_json(&staging.join(SENT), &sent)?;
            std::fs::rename(&staging, &target).map_err(|e| Error::io(&target, e))
        })();
        if let Err(e) = brought {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
        out.push(format!("{project}/{id}"));
    }
    Ok(out)
}

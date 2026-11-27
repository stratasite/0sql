//! The backup: with an object store, each dataset's files are copied
//! under `datasets/<org>/<project>/<id>/` there, and read back only by
//! a restore.
//!
//! What a dataset has is immutable but for two files, so a copy sends
//! what it has not sent before, the edit log, and the manifest last: a
//! dataset in the store is whole once its `dataset.json` is there, and
//! a restore takes only those. A copy that fails is tried again, and
//! then the dataset says it is not backed up; the job it followed has
//! still succeeded. A delete removes the copy too.

use crate::error::{Error, Result};
use crate::store;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use zsql_exec::Objects;

/// Where datasets go in the store.
pub const PREFIX: &str = "datasets";

/// The files sent already, kept beside them, so a copy sends only what
/// is new.
const SENT: &str = ".backed-up.json";

/// The files of a dataset that change: sent every time.
const CHANGING: [&str; 2] = [store::MANIFEST, crate::edits::LOG];

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

/// A dataset's org, project and id, from its directory:
/// `<org>/<project>/.datasets/<id>`.
pub(crate) fn names_of(dir: &Path) -> Option<(String, String, String)> {
    let id = dir.file_name()?.to_string_lossy().to_string();
    let project_dir = dir.parent()?.parent()?;
    let project = project_dir.file_name()?.to_string_lossy().to_string();
    let org = project_dir.parent()?.file_name()?.to_string_lossy().to_string();
    Some((org, project, id))
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

fn key(org: &str, project: &str, id: &str, rel: &str) -> Vec<String> {
    let mut k = vec![PREFIX.to_string(), org.to_string(), project.to_string(), id.to_string()];
    k.extend(rel.split('/').map(str::to_string));
    k
}

fn put(objects: &Objects, path: &[String], file: &Path) -> Result<()> {
    let bytes = std::fs::read(file).map_err(|e| Error::io(file, e))?;
    let parts: Vec<&str> = path.iter().map(String::as_str).collect();
    tried(|| objects.put(&parts, bytes.clone())).map(|_| ())
}

/// Copies what a dataset has not sent yet, the manifest last.
pub(crate) fn copy(objects: &Objects, dir: &Path) -> Result<()> {
    let (org, project, id) = names_of(dir).ok_or_else(|| Error::Internal(format!("{} is not a dataset's directory", dir.display())))?;
    let sent_path = dir.join(SENT);
    let mut sent: BTreeSet<String> = store::read_json(&sent_path).unwrap_or_default();
    for rel in files(dir) {
        if rel == store::MANIFEST || (sent.contains(&rel) && !CHANGING.contains(&rel.as_str())) {
            continue;
        }
        put(objects, &key(&org, &project, &id, &rel), &dir.join(&rel))?;
        if !CHANGING.contains(&rel.as_str()) {
            sent.insert(rel);
        }
    }
    store::write_json(&sent_path, &sent)?;
    put(objects, &key(&org, &project, &id, store::MANIFEST), &dir.join(store::MANIFEST))
}

/// Removes a dataset's copy.
pub(crate) fn forget(objects: &Objects, org: &str, project: &str, id: &str) -> Result<()> {
    tried(|| objects.delete_under(&[PREFIX, org, project, id])).map(|_| ())
}

/// Brings back every dataset of an org the store has whole and this
/// node does not; the ones brought back, as `project/id`.
pub(crate) fn restore(objects: &Objects, data_dir: &Path, org: &str) -> Result<Vec<String>> {
    let keys = tried(|| objects.list(&[PREFIX, org]))?;
    let mut by_dataset: std::collections::BTreeMap<(String, String), Vec<Vec<String>>> = std::collections::BTreeMap::new();
    for k in keys {
        if k.len() < 3 {
            continue;
        }
        by_dataset.entry((k[0].clone(), k[1].clone())).or_default().push(k[2..].to_vec());
    }
    let mut out = Vec::new();
    for ((project, id), rels) in by_dataset {
        if !crate::is_id(&id) || !rels.iter().any(|r| r.len() == 1 && r[0] == store::MANIFEST) {
            continue;
        }
        let root = data_dir.join(org).join(&project).join(store::DIR);
        let target = root.join(&id);
        if target.join(store::MANIFEST).exists() {
            continue;
        }
        let staging = root.join(format!(".restore-{id}-{}", crate::time::nanos()));
        let brought = (|| -> Result<()> {
            for rel in &rels {
                if rel.iter().any(|p| p == ".." || p.is_empty() || p.starts_with('.')) {
                    continue;
                }
                let mut path: Vec<&str> = vec![PREFIX, org, &project, &id];
                path.extend(rel.iter().map(String::as_str));
                let bytes = tried(|| objects.get(&path))?;
                store::write_file(&staging.join(rel.join("/")), &bytes)?;
            }
            // What was sent is what is here: a later copy sends nothing again.
            let sent: BTreeSet<String> = rels.iter().map(|r| r.join("/")).filter(|r| !CHANGING.contains(&r.as_str())).collect();
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

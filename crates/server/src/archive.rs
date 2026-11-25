//! A deploy, kept: when the server has an object store, each deploy is
//! put there as a zip named for its org, project, branch and the time,
//! `deploys/<org>-<project>-<branch>-<time>.zip`, so what a branch was
//! at any deploy can be had back after the next one replaced it.
//!
//! ```text
//! deploy.json            what the deploy answered: counts, warnings, test results
//! project/...            the files the model was loaded from, as they were sent
//! compiled/...           what the server keeps of it: the snapshot, the values, the tests
//! ```
//!
//! No secret goes with it. The store is read by more than the server,
//! and kept longer than a credential is good for, so every secret
//! setting of a datasource is left out of both `datasources.yml` and
//! the snapshot, by the same generous test that decides what is served.
//! Only the files the loader read are taken from the archive that was
//! sent: whatever else it carried, a `.zsql` among it, stays behind.

use serde_json::Value;
use std::io::Write;
use std::path::Path;
use zsql_core::{is_secret, Model};
use zsql_exec::Objects;

/// What every kept deploy's key starts with.
pub(crate) const PREFIX: &str = "deploys";

/// The name a deploy is kept under. None of the three names can hold a
/// `/`; each may hold a `-`, so the time is read from the end.
pub(crate) fn name_of(org: &str, uid: &str, branch: &str, at: std::time::SystemTime) -> String {
    format!("{org}-{uid}-{branch}-{}.zip", zsql_core::utc_stamp(at))
}

/// Puts the deploy in the store and says where it went. `root` is the
/// project as it was unpacked, `scratch` a directory of this deploy's
/// own to write in, and `answer` what the deploy is about to say.
pub(crate) fn keep(
    objects: &Objects,
    (org, uid, branch): (&str, &str, &str),
    root: &Path,
    scratch: &Path,
    model: &Model,
    answer: &Value,
) -> Result<String, String> {
    let bytes = zipped(org, root, scratch, model, answer)?;
    objects.put(PREFIX, &name_of(org, uid, branch, std::time::SystemTime::now()), bytes).map_err(|e| e.to_string())
}

/// The zip of a deploy, with no secret in it.
pub(crate) fn zipped(org: &str, root: &Path, scratch: &Path, model: &Model, answer: &Value) -> Result<Vec<u8>, String> {
    let failed = |what: &str, e: &dyn std::fmt::Display| format!("{what}: {e}");
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let mut add = |name: &str, bytes: &[u8]| -> Result<(), String> {
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).unix_permissions(0o644);
        zip.start_file(name, options).map_err(|e| failed(name, &e))?;
        zip.write_all(bytes).map_err(|e| failed(name, &e))
    };

    let mut said = answer.clone();
    said["org"] = Value::String(org.to_string());
    add("deploy.json", &serde_json::to_vec_pretty(&said).map_err(|e| failed("deploy.json", &e))?)?;

    // The loader names what it read, directories and files that might
    // have been there among them; the files that are there are the
    // project.
    let mut files: Vec<&Path> =
        model.sources.iter().filter_map(|p| p.strip_prefix(root).ok()).filter(|rel| root.join(rel).is_file()).collect();
    files.sort();
    files.dedup();
    for rel in files {
        let name = rel.to_string_lossy().replace('\\', "/");
        let bytes = std::fs::read(root.join(rel)).map_err(|e| failed(&name, &e))?;
        let bytes = if name == "datasources.yml" { without_secrets(&bytes).map_err(|e| failed(&name, &e))? } else { bytes };
        add(&format!("project/{name}"), &bytes)?;
    }

    let compiled = scratch.join("compiled");
    model.write_without_secrets(&compiled).map_err(|e| failed("the compiled model", &e))?;
    let mut kept: Vec<_> =
        std::fs::read_dir(&compiled).map_err(|e| failed("the compiled model", &e))?.flatten().map(|e| e.path()).collect();
    kept.sort();
    for path in kept {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        add(&format!("compiled/{name}"), &std::fs::read(&path).map_err(|e| failed(&name, &e))?)?;
    }
    drop(add);
    Ok(zip.finish().map_err(|e| failed("the zip", &e))?.into_inner())
}

/// `datasources.yml` with every secret setting left out, at any depth.
fn without_secrets(yaml: &[u8]) -> Result<Vec<u8>, serde_yaml::Error> {
    fn strip(value: &mut serde_yaml::Value) {
        match value {
            serde_yaml::Value::Mapping(map) => {
                map.retain(|key, _| !key.as_str().is_some_and(is_secret));
                map.iter_mut().for_each(|(_, inner)| strip(inner));
            }
            serde_yaml::Value::Sequence(items) => items.iter_mut().for_each(strip),
            serde_yaml::Value::Tagged(tagged) => strip(&mut tagged.value),
            _ => {}
        }
    }
    let mut value: serde_yaml::Value = serde_yaml::from_slice(yaml)?;
    strip(&mut value);
    serde_yaml::to_string(&value).map(String::into_bytes)
}

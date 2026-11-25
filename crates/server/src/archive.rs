//! A deploy, kept: when the server has an object store, each deploy is
//! put there as a zip named for its org, project, branch and the time,
//! `deploys/<org>/<project>/<branch>/<org>-<project>-<branch>-<time>.zip`,
//! so what a branch was at any deploy can be had back after the next
//! one replaced it. The directories are there because a name may hold
//! a `-`: two branches of two orgs can spell the same file name, and
//! never the same path.
//!
//! ```text
//! deploy.json            what the deploy answered: counts, warnings, test results
//! project/...            the files the model was loaded from, as they were sent
//! compiled/...           what the server keeps of it: the snapshot, the values, the tests
//! ```
//!
//! No secret is meant to go with it. The store is read by more than
//! the server, and kept longer than a credential is good for, so every
//! setting of a datasource that names a secret is left out of both
//! `datasources.yml` and the snapshot, and a URL loses its password.
//! That is a test of names, wider than the one that decides what is
//! served and still a test of names: a credential under a key nobody
//! would take for one is kept as it was written. Only the files the
//! loader read are taken from the archive that was sent: whatever else
//! it carried, a `.zsql` among it, stays behind.

use serde_json::Value;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use zsql_core::{is_secret_to_a_copy, without_url_secrets, Model};

/// What every kept deploy's key starts with.
pub(crate) const PREFIX: &str = "deploys";

/// Where a deploy is kept, beneath the store's own prefix.
pub(crate) fn path_of(org: &str, uid: &str, branch: &str, at: std::time::SystemTime) -> [String; 5] {
    let name = format!("{org}-{uid}-{branch}-{}.zip", zsql_core::utc_stamp(at));
    [PREFIX.to_string(), org.to_string(), uid.to_string(), branch.to_string(), name]
}

/// A path inside the project with its `.` and `..` worked out, or
/// nothing when it would leave the project: an import is recorded as
/// it was written, `models/../shared/x.yml`, and a zip entry of that
/// name is one an extractor rewrites.
fn within(rel: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in rel.components() {
        match part {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

/// The zip of a deploy, with no secret in it. `root` is the project as
/// it was unpacked, `scratch` a directory of this deploy's own to
/// write in, and `answer` what the deploy is about to say.
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
    let mut files: Vec<PathBuf> =
        model.sources.iter().filter_map(|p| within(p.strip_prefix(root).ok()?)).filter(|rel| root.join(rel).is_file()).collect();
    files.sort();
    files.dedup();
    for rel in files {
        let name = rel.to_string_lossy().replace('\\', "/");
        let bytes = std::fs::read(root.join(&rel)).map_err(|e| failed(&name, &e))?;
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
    Ok(zip.finish().map_err(|e| failed("the zip", &e))?.into_inner())
}

/// `datasources.yml` with every secret setting left out of each
/// datasource's entry, by the test the snapshot's copy is put to. The
/// keys of the file itself are the datasources' own names and are never
/// taken for a setting: a warehouse called `token_store` is a warehouse.
/// Read as the loader reads it, so a file that deploys is one that can
/// be kept.
fn without_secrets(yaml: &[u8]) -> Result<Vec<u8>, String> {
    fn strip(value: &mut serde_yaml::Value) {
        match value {
            serde_yaml::Value::Mapping(map) => {
                map.retain(|key, _| !key.as_str().is_some_and(is_secret_to_a_copy));
                map.iter_mut().for_each(|(_, inner)| strip(inner));
            }
            serde_yaml::Value::Sequence(items) => items.iter_mut().for_each(strip),
            serde_yaml::Value::Tagged(tagged) => strip(&mut tagged.value),
            serde_yaml::Value::String(text) => *text = without_url_secrets(text),
            _ => {}
        }
    }
    let mut value: serde_yaml::Value = serde_yaml::from_slice(yaml).map_err(|e| e.to_string())?;
    if let serde_yaml::Value::Mapping(datasources) = &mut value {
        datasources.iter_mut().for_each(|(_, entry)| strip(entry));
    }
    serde_yaml::to_string(&value).map(String::into_bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_worked_out_inside_the_project_or_refused() {
        assert_eq!(within(Path::new("models/../shared/./x.yml")), Some(PathBuf::from("shared/x.yml")));
        assert_eq!(within(Path::new("models/a.yml")), Some(PathBuf::from("models/a.yml")));
        assert_eq!(within(Path::new("../x.yml")), None);
        assert_eq!(within(Path::new("/etc/passwd")), None);
    }

    #[test]
    fn a_datasource_keeps_its_name_and_loses_its_secrets() {
        let yaml = [
            "token_store:",
            "  adapter: postgres",
            "  url: postgres://reader:hunt/er2@db.internal:5432/shop?sslmode=require&password=hunter2#top",
            "  jdbc_url: jdbc:sqlserver://sql.internal:1433;database=shop;password=hunter2;encrypt=true",
            "  endpoint: https://hunter2@api.internal/v1",
            "  passphrase: hunter2",
            "  Password: hunter2",
            "  private-key: hunter2",
            "  apiKey: hunter2",
            "  key: hunter2",
            "  keyfile_json: '{\"k\": \"hunter2\"}'",
            "  note: !env HOME",
            "  private_key_file: /keys/a.p8",
            "",
        ]
        .join("\n");
        let out = String::from_utf8(without_secrets(yaml.as_bytes()).unwrap()).unwrap();
        assert!(!out.contains("hunter2") && !out.contains("hunt/er2"), "{out}");
        for kept in [
            "token_store:",
            "adapter: postgres",
            "postgres://db.internal:5432/shop?sslmode=require#top",
            "jdbc:sqlserver://sql.internal:1433;database=shop;encrypt=true",
            "https://api.internal/v1",
            "/keys/a.p8",
            "!env HOME",
        ] {
            assert!(out.contains(kept), "lost {kept}:\n{out}");
        }
    }
}

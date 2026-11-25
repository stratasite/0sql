//! A project archive in, a model out: unpacked within limits, loaded,
//! tested, written beside the branch and swapped in whole. And the
//! removal of a branch, which takes the same lock.

use crate::error::{not_found, ApiError, Org, Params};
use crate::guard::Held;
use crate::names::{check_branch, check_uid};
use crate::{blocking, lock, write, Shared};
use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use zsql_core::{Model, Trust};

pub(crate) async fn deploy(
    State(app): State<Shared>,
    held: Option<Extension<Held>>,
    Org(org): Org,
    Params((uid, branch)): Params<(String, String)>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    blocking(move || {
        let _held = held;
        deploy_or_validate(app, org, uid, branch, body, true)
    })
    .await
}

pub(crate) async fn validate(
    State(app): State<Shared>,
    held: Option<Extension<Held>>,
    Org(org): Org,
    Params((uid, branch)): Params<(String, String)>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    blocking(move || {
        let _held = held;
        deploy_or_validate(app, org, uid, branch, body, false)
    })
    .await
}

fn deploy_or_validate(app: Shared, org: String, uid: String, branch: String, body: Bytes, keep: bool) -> Result<Json<Value>, ApiError> {
    let bad = |m: String| ApiError(StatusCode::BAD_REQUEST, "DeployError", m);
    if body.is_empty() {
        return Err(bad("the body must be a tar.gz of the project directory".into()));
    }
    // Before the archive is unpacked: a bad name is answered without
    // doing work.
    check_uid(&uid)?;
    check_branch(&branch)?;
    // Named by a counter, not the clock: two deploys in one process at
    // the same instant must not share a directory, and the second to
    // unpack into a shared one would be refused for files that exist.
    static WORK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = WORK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let work = std::env::temp_dir().join(format!("zsqld-deploy-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&work).map_err(|e| bad(e.to_string()))?;
    // Paths in what the loader says are inside the work directory; the
    // caller knows their project by its own layout, not by where the
    // server unpacked it.
    let work_prefix = format!("{}/", work.display());
    let result = (|| -> Result<Value, ApiError> {
        unpack_archive(&body, &work).map_err(|m| bad(m.replace(&work_prefix, "")))?;
        let root = project_root(&work).ok_or_else(|| bad("the archive holds no project.yml".into()))?;
        let loaded = zsql_core::load(&root, Trust::Untrusted, app.dialects).map_err(|e| bad(e.to_string().replace(&work_prefix, "")))?;
        let deployed =
            Arc::new(Model::from_loaded(&uid, &branch, loaded, app.dialects).map_err(|e| bad(e.to_string().replace(&work_prefix, "")))?);
        let mut out = deployed.summary();
        out["test_results"] = deployed.run_tests()?;
        out["validated_only"] = Value::Bool(!keep);
        if keep {
            // Kept in the object store first, and outside the lock: a
            // deploy that cannot be kept is refused before it replaces
            // anything, and a slow store holds up no other deploy.
            if let Some(objects) = app.runs.objects() {
                let scratch = work.with_file_name(format!("zsqld-deploy-{}-{seq}-kept", std::process::id()));
                let kept = crate::archive::keep(objects, (&org, &uid, &branch), &root, &scratch, &deployed, &out);
                let _ = std::fs::remove_dir_all(&scratch);
                let key = kept.map_err(|m| {
                    let said = format!("the deploy could not be kept in the object store, and was not made: {m}");
                    ApiError(StatusCode::BAD_GATEWAY, "ArchiveError", said.replace(&work_prefix, ""))
                })?;
                tracing::info!("kept {org}/{uid}/{branch} in the object store at {key}");
                out["archive"] = Value::String(key);
            }
            // Keep the compiled model on disk so a restart reloads it:
            // written beside the branch, then swapped in whole.
            let _writing = lock(&app.deploying);
            let live = app.dir(&org, &uid, &branch);
            let staged = live.with_file_name(format!(".{branch}.staging-{}", nanos()));
            deployed.write(&staged).and_then(|()| swap_in(&staged, &live)).map_err(|e| {
                let _ = std::fs::remove_dir_all(&staged);
                bad(e.to_string())
            })?;
            write(&app.models).insert(crate::key(&org, &uid, &branch), deployed);
            tracing::info!("deployed {org}/{uid}/{branch}");
        }
        Ok(out)
    })();
    let _ = std::fs::remove_dir_all(&work);
    result.map(Json)
}

/// The most an archive may hold once decompressed, and the most entries.
/// The body limit bounds what is sent; gzip can grow a thousandfold on
/// the way out, into a temp directory that is memory on the hosted box.
pub(crate) const UNPACKED_LIMIT: u64 = 256 * 1024 * 1024;
const ENTRY_LIMIT: usize = 20_000;

/// A reader that stops with an error, not a quiet end, past a limit, so
/// the archive reader's message says what happened.
pub(crate) struct Capped<R> {
    pub(crate) inner: R,
    pub(crate) left: u64,
    /// Set when the limit was hit, for the caller to tell that apart
    /// from any other failure the archive reader wraps it in.
    pub(crate) tripped: Arc<std::sync::atomic::AtomicBool>,
}

impl<R: std::io::Read> std::io::Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            self.tripped.store(true, std::sync::atomic::Ordering::Relaxed);
            return Err(std::io::Error::other("decompressed size limit"));
        }
        let want = buf.len().min(self.left as usize);
        let n = self.inner.read(&mut buf[..want])?;
        self.left -= n as u64;
        Ok(n)
    }
}

/// Unpacks the project archive into `work`: regular files and
/// directories only, within the size and count limits.
///
/// A symbolic or hard link is refused rather than created. The loader
/// follows links when it reads, so a link to another project's
/// directory, or to anything on the host, would be read as part of the
/// project and shown back in the model or its errors. The CLI never
/// writes one.
fn unpack_archive(body: &[u8], work: &std::path::Path) -> Result<(), String> {
    let tripped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let decoder = Capped { inner: flate2::read::GzDecoder::new(body), left: UNPACKED_LIMIT, tripped: tripped.clone() };
    let mut archive = tar::Archive::new(decoder);
    archive.set_overwrite(false);
    let too_big = || format!("the archive is larger than {} MiB when decompressed", UNPACKED_LIMIT / (1024 * 1024));
    let failed = |what: &str, e: std::io::Error| {
        if tripped.load(std::sync::atomic::Ordering::Relaxed) {
            too_big()
        } else {
            format!("{what} the archive: {e}")
        }
    };
    let entries = archive.entries().map_err(|e| failed("reading", e))?;
    let mut n = 0usize;
    for entry in entries {
        let mut entry = entry.map_err(|e| failed("reading", e))?;
        n += 1;
        if n > ENTRY_LIMIT {
            return Err(format!("the archive holds more than {ENTRY_LIMIT} entries"));
        }
        let kind = entry.header().entry_type();
        if !matches!(kind, tar::EntryType::Regular | tar::EntryType::Directory) {
            let path = entry.path().map(|p| p.display().to_string()).unwrap_or_default();
            return Err(format!("the archive holds {path}, which is not a file or a directory ({kind:?}); only those are deployed"));
        }
        if !entry.unpack_in(work).map_err(|e| failed("unpacking", e))? {
            let path = entry.path().map(|p| p.display().to_string()).unwrap_or_default();
            return Err(format!("the archive holds {path}, which would land outside the project; only paths inside it are deployed"));
        }
    }
    Ok(())
}

/// The directory holding project.yml: the archive root, or its single top folder.
fn project_root(work: &std::path::Path) -> Option<PathBuf> {
    if work.join("project.yml").exists() {
        return Some(work.to_path_buf());
    }
    let entries: Vec<PathBuf> = std::fs::read_dir(work).ok()?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect();
    entries.into_iter().find(|p| p.join("project.yml").exists())
}

pub(crate) async fn remove(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, branch)): Params<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    check_uid(&uid)?;
    check_branch(&branch)?;
    blocking(move || {
        // Under the deploy lock: a deploy of the same branch in flight
        // would otherwise swap its directory in between the removal of
        // the model and of the files.
        let _writing = lock(&app.deploying);
        let removed = write(&app.models).remove(&crate::key(&org, &uid, &branch)).is_some();
        if !removed {
            return Err(not_found(&org, &uid, &branch));
        }
        let _ = std::fs::remove_dir_all(app.dir(&org, &uid, &branch));
        tracing::info!("removed {org}/{uid}/{branch}");
        // Named as the log names it, with the org when it is not the default.
        let within = if org == crate::names::DEFAULT_ORG { String::new() } else { format!("{org}/") };
        Ok(Json(json!({ "removed": format!("{within}{uid}/{branch}") })))
    })
    .await
}

pub(crate) fn nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

/// What a deploy leaves beside a branch while it swaps:
/// `.<branch>.staging-<n>`, then `.<branch>.previous-<n>`.
pub(crate) fn is_swap_leftover(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.') else { return false };
    [".staging-", ".previous-"]
        .iter()
        .any(|mark| rest.rsplit_once(mark).is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())))
}

/// Puts `staged` where `live` is, by rename, so a reader of the data
/// directory sees the branch as it was or as it is, never half
/// written. A leftover `.previous` directory is swept at startup.
fn swap_in(staged: &std::path::Path, live: &std::path::Path) -> std::io::Result<()> {
    let name = live.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let previous = live.with_file_name(format!(".{name}.previous-{}", nanos()));
    let had_live = live.exists();
    if had_live {
        std::fs::rename(live, &previous)?;
    }
    if let Err(e) = std::fs::rename(staged, live) {
        if had_live {
            let _ = std::fs::rename(&previous, live);
        }
        return Err(e);
    }
    if had_live {
        let _ = std::fs::remove_dir_all(&previous);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a deploy's own leftovers are swept at startup.
    #[test]
    fn only_swap_leftovers_are_swept() {
        assert!(is_swap_leftover(".main.staging-17"));
        assert!(is_swap_leftover(".release-1.2.previous-1700000000"));
        assert!(!is_swap_leftover(".git"));
        assert!(!is_swap_leftover(".main.staging-"));
        assert!(!is_swap_leftover(".main.staging-x"));
        assert!(!is_swap_leftover("main"));
    }

    /// The swap leaves the new branch, whole, and nothing else.
    #[test]
    fn a_deploy_swaps_in_whole() {
        let root = std::env::temp_dir().join(format!("zsqld-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let live = root.join("main");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join("old"), "1").unwrap();
        let staged = root.join(".main.staging-1");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("new"), "2").unwrap();
        swap_in(&staged, &live).unwrap();
        assert!(live.join("new").exists() && !live.join("old").exists());
        let names: Vec<String> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["main"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}

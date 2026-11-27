//! A project archive in, a model out: unpacked within limits, loaded,
//! tested, written beside the branch and swapped in whole. And the
//! removal of a branch, which takes the same lock, and the making of
//! an org, which is the one thing a deploy may never do.

use crate::error::{not_found, ApiError, Input, Org, Params, Search};
use crate::guard::Held;
use crate::names::{check_branch, check_org, check_uid};
use crate::{blocking, lock, write, Shared};
use axum::body::{Body, Bytes};
use axum::extract::{Extension, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use zsql_core::{Model, Trust};

pub(crate) async fn deploy(
    State(app): State<Shared>,
    held: Option<Extension<Held>>,
    Org(org): Org,
    Params((uid, branch)): Params<(String, String)>,
    Search(q): Search<DeployQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    answer(app, held, Asked { org, uid, branch, body, keep: Some(q) }, wants_stream(&headers)).await
}

pub(crate) async fn validate(
    State(app): State<Shared>,
    held: Option<Extension<Held>>,
    Org(org): Org,
    Params((uid, branch)): Params<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    answer(app, held, Asked { org, uid, branch, body, keep: None }, wants_stream(&headers)).await
}

/// The media type of a deploy told as it goes: one JSON object a line.
pub(crate) const NDJSON: &str = "application/x-ndjson";

/// Whether the caller asked to be told each step as it is done.
fn wants_stream(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.split(';').next().is_some_and(|t| t.trim().eq_ignore_ascii_case(NDJSON)))
}

/// What a deploy or a validation was asked to do.
struct Asked {
    org: String,
    uid: String,
    branch: String,
    body: Bytes,
    /// How a deploy is kept; without it this is a validation.
    keep: Option<DeployQuery>,
}

/// The whole answer at the end, or, when `stream`, a line for each step
/// as it is done and the answer as the last line.
///
/// A stream's status is 200 once the first line is sent, so a refusal
/// after that is a line of its own: `{"event":"error","status":...,
/// "error":{"class","message"}}`. What can be refused before any work is
/// refused with its status either way. A caller that goes away does not
/// stop the deploy: it runs to its end, made or not, as one whose answer
/// was never read would.
async fn answer(app: Shared, held: Option<Extension<Held>>, asked: Asked, stream: bool) -> Result<Response, ApiError> {
    if !stream {
        return blocking(move || {
            let _held = held;
            deploy_or_validate(app, asked, &mut |_| {})
        })
        .await
        .map(|v| Json(v).into_response());
    }
    precheck(&asked.body, &asked.uid, &asked.branch)?;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(16);
    tokio::task::spawn_blocking(move || {
        let _held = held;
        let send = |v: Value| {
            let mut line = serde_json::to_vec(&v).unwrap_or_default();
            line.push(b'\n');
            // Nobody reading is no reason to stop half way.
            let _ = tx.blocking_send(Ok(line));
        };
        let last = match deploy_or_validate(app, asked, &mut |event| send(event)) {
            Ok(v) => json!({ "event": "done", "result": v }),
            Err(ApiError(status, class, message)) => {
                json!({ "event": "error", "status": status.as_u16(), "error": { "class": class, "message": message } })
            }
        };
        send(last);
    });
    let mut response = Response::new(Body::from_stream(crate::exec::Chunks(rx)));
    response.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static(NDJSON));
    // A proxy that buffers would hold every line to the end.
    response.headers_mut().insert("x-accel-buffering", header::HeaderValue::from_static("no"));
    Ok(response)
}

/// What is refused before an archive is unpacked: answered without doing
/// work, and, on a stream, with its status rather than a line.
fn precheck(body: &[u8], uid: &str, branch: &str) -> Result<(), ApiError> {
    if body.is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "DeployError", "the body must be a tar.gz of the project directory".into()));
    }
    check_uid(uid)?;
    check_branch(branch)?;
    Ok(())
}

/// How a deploy is to treat what the branch already has.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeployQuery {
    /// This deploy is the whole truth about the branch's credentials: a
    /// datasource that arrives with no secret is left with none, where
    /// it would otherwise keep the one it had.
    #[serde(default)]
    clear_secrets: bool,
}

/// `keep` is how a deploy is kept; without it this is a validation,
/// which keeps nothing. `on` is told each step as it is done, with the
/// milliseconds since the start: `unpacked`, `loaded`, `compiled`, a
/// `test` for each test and `tested`, then, for a deploy, `kept` when
/// there is an object store, and `live`.
fn deploy_or_validate(app: Shared, asked: Asked, on: &mut dyn FnMut(Value)) -> Result<Value, ApiError> {
    let Asked { org, uid, branch, body, keep } = asked;
    let bad = |m: String| ApiError(StatusCode::BAD_REQUEST, "DeployError", m);
    let started = std::time::Instant::now();
    let mut step = |event: &str, mut v: Value| {
        v["event"] = json!(event);
        v["ms"] = json!(started.elapsed().as_millis() as u64);
        on(v);
    };
    precheck(&body, &uid, &branch)?;
    // Named by a counter, not the clock: two deploys in one process at
    // the same instant must not share a directory, and the second to
    // unpack into a shared one would be refused for files that exist.
    static WORK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = WORK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let work = std::env::temp_dir().join(format!("zsqld-deploy-{}-{seq}", std::process::id()));
    // The archive carries secrets now, so the directory is this
    // user's alone and is one nobody made first: an existing one is
    // somebody else's, or a leftover, and is not unpacked into.
    let _ = std::fs::remove_dir_all(&work);
    let mut made = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        made.mode(0o700);
    }
    made.create(&work).map_err(|e| bad(format!("a directory to unpack into: {e}")))?;
    // Removed however this ends, a panic in a load or a test included.
    let _swept = Swept(work.clone());
    // Paths in what the loader says are inside the work directory; the
    // caller knows their project by its own layout, not by where the
    // server unpacked it.
    let work_prefix = format!("{}/", work.display());
    let result = (|| -> Result<Value, ApiError> {
        let entries = unpack_archive(&body, &work).map_err(|m| bad(m.replace(&work_prefix, "")))?;
        step("unpacked", json!({ "bytes": body.len(), "entries": entries }));
        let root = project_root(&work).ok_or_else(|| bad("the archive holds no project.yml".into()))?;
        let loaded = zsql_core::load(&root, Trust::Untrusted, app.dialects).map_err(|e| bad(e.to_string().replace(&work_prefix, "")))?;
        step("loaded", json!({}));
        let mut deployed =
            Model::from_loaded(&uid, &branch, loaded, app.dialects).map_err(|e| bad(e.to_string().replace(&work_prefix, "")))?;
        let mut out = deployed.summary();
        let counts = ["tables", "fields", "joins", "paths", "policies", "values", "tests"];
        step("compiled", Value::Object(counts.iter().map(|k| (k.to_string(), out[*k].clone())).collect()));
        let tested = deployed.run_tests()?;
        for r in tested["results"].as_array().into_iter().flatten() {
            step("test", json!({ "name": r["name"], "status": r["status"] }));
        }
        step("tested", json!({ "passed": tested["passed"], "failed": tested["failed"] }));
        out["test_results"] = tested;
        out["validated_only"] = Value::Bool(keep.is_none());
        // What is kept in the object store is made ready outside
        // the lock, since zipping a project is the slow half and
        // holds up no other deploy there. In a directory beside
        // the unpacked archive and never inside it: nothing the
        // deployer sent is in there to be taken for the model.
        let zip = match app.runs.objects().filter(|_| keep.is_some()) {
            Some(_) => {
                // This user's alone and nobody's before, as the directory
                // the archive is unpacked into is: what is in it goes
                // into the zip, so nobody else may put anything there.
                let scratch = work.with_file_name(format!("zsqld-deploy-{}-{seq}-kept", std::process::id()));
                let _ = std::fs::remove_dir_all(&scratch);
                made.create(&scratch).map_err(|e| bad(format!("a directory to make the zip in: {e}")))?;
                let _swept = Swept(scratch.clone());
                let zipped = crate::archive::zipped(&root, &scratch, &deployed);
                Some(zipped.map_err(|m| {
                    let said = format!("the deploy could not be made ready to keep, and was not made: {m}");
                    let said = said.replace(&work_prefix, "").replace(&format!("{}/", scratch.display()), "");
                    ApiError(StatusCode::INTERNAL_SERVER_ERROR, "ArchiveError", said)
                })?)
            }
            None => None,
        };
        if let Some(how) = keep {
            // Keep the compiled model on disk so a restart reloads it:
            // written beside the branch, then swapped in whole.
            let _writing = lock(&app.deploying);
            // Under the lock, so the model whose credentials are kept
            // is the one this deploy replaces.
            let previous = crate::read(&app.models).get(&crate::key(&org, &uid, &branch)).cloned();
            out["credentials"] = crate::credentials::settle(&mut deployed, previous.as_deref(), how.clear_secrets);
            let deployed = Arc::new(deployed);
            let live = app.dir(&org, &uid, &branch);
            let staged = live.with_file_name(format!(".{branch}.staging-{}", nanos()));
            let unstaged = |e: std::io::Error| {
                let _ = std::fs::remove_dir_all(&staged);
                bad(e.to_string())
            };
            deployed.write(&staged).map_err(unstaged)?;
            // Put under the lock, between the write and the swap, and
            // named for this moment: the zips of a branch are in the
            // order its deploys went live, a deploy the disk refused
            // leaves none, and one the store refuses replaces nothing.
            // Put only where nothing is: another node, or a clock set
            // back, may have this very name, and then the next moment's
            // is taken.
            let mut kept = None;
            if let (Some(objects), Some(zip)) = (app.runs.objects(), zip) {
                let mut tries = 0;
                let (path, key) = loop {
                    let path = crate::archive::path_of(&org, &uid, &branch, std::time::SystemTime::now());
                    let parts: Vec<&str> = path.iter().map(String::as_str).collect();
                    let unkept = |why: String| {
                        let _ = std::fs::remove_dir_all(&staged);
                        // The store's own words name where it is; the log
                        // has them, and the answer what was being put.
                        tracing::warn!("{org}/{uid}/{branch} could not be kept in the object store: {why}");
                        let said = format!(
                            "the deploy could not be kept in the object store, and was not made: {} could not be put there",
                            path.join("/")
                        );
                        ApiError(StatusCode::BAD_GATEWAY, "ArchiveError", said)
                    };
                    // The answer is whole now, credentials and all, and
                    // goes into the zip as it will be given.
                    out["archive"] = Value::String(path.join("/"));
                    let whole = crate::archive::with_answer(zip.clone(), &org, &out).map_err(|m| {
                        let _ = std::fs::remove_dir_all(&staged);
                        ApiError(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "ArchiveError",
                            format!("the deploy could not be made ready to keep, and was not made: {m}"),
                        )
                    })?;
                    match objects.put_new(&parts, whole) {
                        Ok(Some(key)) => break (path.join("/"), key),
                        Ok(None) if tries < 5 => {
                            tries += 1;
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                        Ok(None) => return Err(unkept("every name tried was taken".into())),
                        Err(e) => return Err(unkept(e.to_string())),
                    }
                };
                step("kept", json!({ "archive": path }));
                kept = Some((objects, key));
            }
            if let Err(e) = swap_in(&staged, &live) {
                // What was put is of a deploy that was not made.
                if let Some((objects, key)) = &kept {
                    if let Err(left) = objects.delete(key) {
                        tracing::warn!("{org}/{uid}/{branch} was not deployed, and its zip is still in the object store at {key}: {left}");
                    }
                }
                return Err(unstaged(e));
            }
            if let Some((_, key)) = &kept {
                tracing::info!("kept {org}/{uid}/{branch} in the object store at {key}");
            }
            write(&app.models).insert(crate::key(&org, &uid, &branch), deployed);
            tracing::info!("deployed {org}/{uid}/{branch}");
            step("live", json!({}));
        }
        Ok(out)
    })();
    result
}

/// A directory removed when this is dropped.
struct Swept(PathBuf);

impl Drop for Swept {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
fn unpack_archive(body: &[u8], work: &std::path::Path) -> Result<usize, String> {
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
    Ok(n)
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
        let loaded = write(&app.models).remove(&crate::key(&org, &uid, &branch)).is_some();
        // A branch on disk that would not load at startup is removed
        // too: it is not served, but it is still the branch's files.
        let dir = app.dir(&org, &uid, &branch);
        let on_disk = dir.is_dir();
        if !loaded && !on_disk {
            return Err(not_found(&org, &uid, &branch));
        }
        if on_disk {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Internal", format!("could not remove {uid}/{branch}: {e}")))?;
        }
        tracing::info!("removed {org}/{uid}/{branch}");
        // Named as the log names it, with the org when it is not the default.
        let within = if org == crate::names::DEFAULT_ORG { String::new() } else { format!("{org}/") };
        Ok(Json(json!({ "removed": format!("{within}{uid}/{branch}") })))
    })
    .await
}

/// A whole project: every branch of it, loaded or not, and its
/// directory. A branch that was on disk but would not load at startup
/// goes too, since it is the project's all the same. What the object
/// store keeps of past deploys stays, as it does for one branch: that
/// is history, and its own store's to expire.
pub(crate) async fn remove_project(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid,)): Params<(String,)>,
) -> Result<Json<Value>, ApiError> {
    check_uid(&uid)?;
    blocking(move || {
        // Under the deploy lock, as for a branch: a deploy in flight
        // would otherwise swap a branch back in under the removal.
        let _writing = lock(&app.deploying);
        let prefix = crate::key(&org, &uid, "");
        let mut branches: Vec<String> = {
            let mut models = write(&app.models);
            let keys: Vec<String> = models.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
            keys.iter().filter_map(|k| models.remove(k).map(|m| m.branch.clone())).collect()
        };
        let dir = app.data_dir.join(&org).join(&uid);
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if e.path().is_dir() && !name.starts_with('.') && !branches.contains(&name) {
                    branches.push(name);
                }
            }
        }
        let existed = dir.exists();
        if branches.is_empty() && !existed {
            let within = if org == crate::names::DEFAULT_ORG { String::new() } else { format!(" in org {org}") };
            return Err(ApiError(StatusCode::NOT_FOUND, "NotFound", format!("no deployment for project {uid}{within}")));
        }
        if existed {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Internal", format!("could not remove {uid}: {e}")))?;
        }
        branches.sort();
        tracing::info!("removed {org}/{uid} and its branches {}", branches.join(", "));
        let within = if org == crate::names::DEFAULT_ORG { String::new() } else { format!("{org}/") };
        Ok(Json(json!({ "removed": format!("{within}{uid}"), "branches": branches })))
    })
    .await
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewOrg {
    org: String,
}

/// `POST /orgs`: make an org. Its folder made by hand is one too, but a
/// deploy never makes one. A deploy
/// to an org nobody made is refused, so an `org:` in project.yml that
/// is mistyped or changed cannot make one, nor move a project to a new
/// name. Made once: an org that exists, `default` among them, is a 409.
pub(crate) async fn create_org(State(app): State<Shared>, Input(new): Input<NewOrg>) -> Result<(StatusCode, Json<Value>), ApiError> {
    let org = new.org;
    check_org(&org)?;
    blocking(move || {
        let taken = || ApiError(StatusCode::CONFLICT, "Conflict", format!("org {org} exists already"));
        if app.has_org(&org) {
            return Err(taken());
        }
        // Not create_dir_all: of two asking at once, one makes it and
        // the other is told it exists.
        match std::fs::create_dir(app.data_dir.join(&org)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(taken()),
            Err(e) => return Err(ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Internal", format!("could not make org {org}: {e}"))),
        }
        tracing::info!("made org {org}");
        Ok((StatusCode::CREATED, Json(json!({ "org": org }))))
    })
    .await
}

/// `DELETE /orgs/{org}`: an org, only once it is empty. Its projects
/// are removed first, each by its own route, so nothing of an org goes
/// with it by accident. `default` is never removed: startup would only
/// make it again.
pub(crate) async fn delete_org(State(app): State<Shared>, Org(org): Org) -> Result<Json<Value>, ApiError> {
    if org == crate::names::DEFAULT_ORG {
        return Err(ApiError(StatusCode::CONFLICT, "Conflict", format!("org {org} is always there and cannot be removed")));
    }
    blocking(move || {
        // Under the deploy lock: a deploy in flight would otherwise
        // write a branch into the directory as it goes.
        let _writing = lock(&app.deploying);
        let dir = app.data_dir.join(&org);
        let internal =
            |e: std::io::Error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Internal", format!("could not remove org {org}: {e}"));
        let mut held: Vec<String> =
            std::fs::read_dir(&dir).map_err(internal)?.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        if !held.is_empty() {
            held.sort();
            return Err(ApiError(
                StatusCode::CONFLICT,
                "Conflict",
                format!("org {org} is not empty: it holds {}; remove its projects first", held.join(", ")),
            ));
        }
        // remove_dir, not remove_dir_all: what lands in it after the
        // look above is refused by the filesystem, not taken with it.
        std::fs::remove_dir(&dir).map_err(internal)?;
        tracing::info!("removed org {org}");
        Ok(Json(json!({ "removed": org })))
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

//! A project archive in, a model out: unpacked within limits, loaded,
//! tested, written beside the branch and swapped in whole. And the
//! removal of a branch, which takes the same lock.

use crate::auth::{user_of, Auth, Held};
use crate::error::{accounts_error, internal, key_required, not_found, ApiError};
use crate::tenancy::{check_branch, check_uid, gate, scope_of, Tenant};
use crate::{blocking, lock, read, write, App, Shared};
use axum::body::Bytes;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use zsql_accounts::{Principal, Verb};
use zsql_service::{Model, Trust};

pub(crate) async fn deploy(
    State(app): State<Shared>,
    Auth(who): Auth,
    held: Option<Extension<Held>>,
    Path((uid, branch)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    blocking(move || {
        let _held = held;
        deploy_or_validate(app, who, uid, branch, body, true)
    })
    .await
}

pub(crate) async fn validate(
    State(app): State<Shared>,
    Auth(who): Auth,
    held: Option<Extension<Held>>,
    Path((uid, branch)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    blocking(move || {
        let _held = held;
        deploy_or_validate(app, who, uid, branch, body, false)
    })
    .await
}

/// Refuses a deploy that would put an account over its branch caps.
///
/// Checked before the archive is unpacked, so going over costs the
/// caller nothing and the server nothing. Redeploying a branch that is
/// already there is not a new branch and is always allowed, which is
/// what keeps an account at its cap from being locked out of its own
/// models.
///
/// Counted from the models actually in memory rather than from the
/// project records, because memory is what is being protected: a branch
/// that is not loaded is not holding anything.
fn check_caps(app: &App, who: &Option<Principal>, uid: &str, branch: &str) -> Result<(), ApiError> {
    let models = read(&app.models);
    caps_allow(app, &models, &scope_of(who), uid, branch)
}

/// The cap check itself, over a models map the caller already holds.
/// Taken twice: once before the work, and once more under the write lock
/// that inserts, because two deploys can pass the first check at the
/// same moment and only the second one is exact.
fn caps_allow(app: &App, models: &HashMap<String, Arc<Model>>, scope: &str, uid: &str, branch: &str) -> Result<(), ApiError> {
    if app.accounts.is_none() {
        return Ok(());
    }
    let (key, project_prefix) = (format!("{scope}{uid}/{branch}"), format!("{scope}{uid}/"));
    if models.contains_key(&key) {
        return Ok(());
    }
    let (in_project, in_account) = branch_counts(models.keys().map(|k| k.as_str()), scope, &project_prefix);
    let over = |what: String, n: usize, cap: usize| {
        ApiError(
            StatusCode::CONFLICT,
            "OverLimit",
            format!(
                "{what} already has {n} deployed branch{} of the {cap} allowed. Remove one before deploying another: `zsql remove --branch <branch> --yes`, or delete it in the web app under the project.",
                if n == 1 { "" } else { "es" }
            ),
        )
    };
    if in_project >= app.caps.per_project {
        return Err(over(uid.to_string(), in_project, app.caps.per_project));
    }
    if in_account >= app.caps.per_account {
        return Err(over("this account".into(), in_account, app.caps.per_account));
    }
    Ok(())
}

/// Deployed branches in this project, and in the whole account, counted
/// from the keys of the models map (`[account/]project/branch`).
///
/// The trailing slash both prefixes carry is load-bearing: without it
/// the account `acme` would count `acme-2`'s branches against itself,
/// and the project `sales` would count `sales-eu`'s.
fn branch_counts<'a>(keys: impl Iterator<Item = &'a str>, scope: &str, project_prefix: &str) -> (usize, usize) {
    let (mut in_project, mut in_account) = (0usize, 0usize);
    for k in keys.filter(|k| k.starts_with(scope)) {
        in_account += 1;
        if k.starts_with(project_prefix) {
            in_project += 1;
        }
    }
    (in_project, in_account)
}

/// A user deploying to a project the account does not have yet creates
/// it and owns it; validating needs no project. Query keys cannot deploy.
fn deploy_gate(
    app: &App,
    who: &Option<Principal>,
    uid: &str,
    branch: &str,
    keep: bool,
    production_branch: &str,
    name: &str,
) -> Result<Tenant, ApiError> {
    check_uid(uid)?;
    check_branch(branch)?;
    let scope = scope_of(who);
    let (store, p) = match (&app.accounts, who) {
        (None, _) => return Ok(Tenant { scope, project: None }),
        (Some(_), None) => return Err(key_required()),
        (Some(store), Some(p)) => (store, p),
    };
    match store.project(p.account().id, uid).map_err(internal)? {
        Some(project) => {
            store.authorize(p, &project, branch, Verb::Write).map_err(accounts_error)?;
            Ok(Tenant { scope, project: Some(project) })
        }
        None => {
            let (user, account, _) = user_of(who)?;
            if !keep {
                return Ok(Tenant { scope, project: None });
            }
            let project = store.create_project(account.id, user, uid, name, production_branch).map_err(accounts_error)?;
            Ok(Tenant { scope, project: Some(project) })
        }
    }
}

fn deploy_or_validate(
    app: Shared,
    who: Option<Principal>,
    uid: String,
    branch: String,
    body: Bytes,
    keep: bool,
) -> Result<Json<Value>, ApiError> {
    let bad = |m: String| ApiError(StatusCode::BAD_REQUEST, "DeployError", m);
    if body.is_empty() {
        return Err(bad("the body must be a tar.gz of the project directory".into()));
    }
    if matches!(who, Some(Principal::QueryKey { .. })) {
        return Err(ApiError(StatusCode::FORBIDDEN, "Forbidden", "query keys are read only; deploy with your personal key".into()));
    }
    // Before the caps are counted under these names, and before the
    // archive is unpacked: a bad name is answered without doing work.
    check_uid(&uid)?;
    check_branch(&branch)?;
    // A validate keeps nothing, so it is not held to the caps.
    if keep {
        check_caps(&app, &who, &uid, &branch)?;
    }
    // A project that already exists says now whether the caller may
    // write it, before the archive is unpacked, loaded and tested on
    // their behalf. A new project is gated once project.yml is read.
    if let (Some(store), Some(p)) = (&app.accounts, &who) {
        if let Some(project) = store.project(p.account().id, &uid).map_err(internal)? {
            store.authorize(p, &project, &branch, Verb::Write).map_err(accounts_error)?;
        }
    }
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
        let loaded = zsql_service::load(&root, Trust::Untrusted, app.dialects).map_err(|e| bad(e.to_string().replace(&work_prefix, "")))?;
        let t = deploy_gate(&app, &who, &uid, &branch, keep, &loaded.snapshot.branch.uid, &loaded.snapshot.project.name)?;
        let deployed =
            Arc::new(Model::from_loaded(&uid, &branch, loaded, app.dialects).map_err(|e| bad(e.to_string().replace(&work_prefix, "")))?);
        let mut out = deployed.summary();
        out["test_results"] = deployed.run_tests()?;
        out["validated_only"] = Value::Bool(!keep);
        if keep {
            // Keep the compiled model on disk so a restart reloads it:
            // written beside the branch, then swapped in whole.
            let _writing = lock(&app.deploying);
            let live = t.dir(&app, &uid, &branch);
            let staged = live.with_file_name(format!(".{branch}.staging-{}", nanos()));
            deployed.write(&staged).and_then(|()| swap_in(&staged, &live)).map_err(|e| {
                let _ = std::fs::remove_dir_all(&staged);
                bad(e.to_string())
            })?;
            let mut models = write(&app.models);
            caps_allow(&app, &models, &t.scope, &uid, &branch)?;
            models.insert(t.key(&uid, &branch), deployed);
            drop(models);
            if let (Some(store), Some(p)) = (&app.accounts, &who) {
                let _ = store.record(p.account().id, &p.describe(), "deploy", &format!("{uid}/{branch}"));
            }
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
/// follows links when it reads, so a link to another account's
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
    Auth(who): Auth,
    Path((uid, branch)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let t = gate(&app, &who, &uid, &branch, Verb::Write)?;
    blocking(move || {
        // Under the deploy lock: a deploy of the same branch in flight
        // would otherwise swap its directory in between the removal of
        // the model and of the files.
        let _writing = lock(&app.deploying);
        let removed = write(&app.models).remove(&t.key(&uid, &branch)).is_some();
        if !removed {
            return Err(not_found(&uid, &branch));
        }
        let _ = std::fs::remove_dir_all(t.dir(&app, &uid, &branch));
        if let (Some(store), Some(p)) = (&app.accounts, &who) {
            let _ = store.record(p.account().id, &p.describe(), "branch_removed", &format!("{uid}/{branch}"));
        }
        Ok(Json(json!({ "removed": format!("{uid}/{branch}") })))
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

    /// The caps are counted by string prefix over the deployment keys,
    /// and a prefix is an easy thing to get subtly wrong: the account
    /// `acme` must not be charged for `acme-2`, and the project `sales`
    /// must not be charged for `sales-eu`. Both of those would show up
    /// as an account mysteriously unable to deploy.
    #[test]
    fn branches_are_counted_against_the_right_project_and_account() {
        let keys =
            ["acme/sales/main", "acme/sales/feature-x", "acme/sales-eu/main", "acme/web/main", "acme-2/sales/main", "acme-2/sales/staging"];
        let it = || keys.iter().copied();

        // Four branches in acme, two of them in acme/sales.
        assert_eq!(branch_counts(it(), "acme/", "acme/sales/"), (2, 4));

        // `sales-eu` is its own project, with its own count.
        assert_eq!(branch_counts(it(), "acme/", "acme/sales-eu/"), (1, 4));

        // The other account is counted separately, and is not reached
        // by a prefix of its name.
        assert_eq!(branch_counts(it(), "acme-2/", "acme-2/sales/"), (2, 2));

        // A project with nothing deployed yet starts at zero.
        assert_eq!(branch_counts(it(), "acme/", "acme/new/"), (0, 4));

        // Single-tenant: no scope at all, so everything is in scope and
        // the account count is simply the total.
        let flat = ["sales/main", "sales/dev", "web/main"];
        assert_eq!(branch_counts(flat.iter().copied(), "", "sales/"), (2, 3));
    }
}

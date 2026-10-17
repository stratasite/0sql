//! Where a request's deployments live, and whether the caller may reach
//! them: the tenant, the gate in front of every branch, the names that
//! become directories, and the context a key is bound to.

use crate::error::{accounts_error, internal, key_required, not_found, ApiError};
use crate::{read, App};
use axum::http::StatusCode;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use zsql_accounts::{Principal, Project, Verb};
use zsql_service::Model;
use zsql_spec::SecurityContext;

/// Where a request's deployments live: the account's slice, or everything.
pub(crate) struct Tenant {
    pub(crate) scope: String,
    pub(crate) project: Option<Project>,
}

impl Tenant {
    pub(crate) fn key(&self, uid: &str, branch: &str) -> String {
        format!("{}{uid}/{branch}", self.scope)
    }
    pub(crate) fn dir(&self, app: &App, uid: &str, branch: &str) -> PathBuf {
        app.data_dir.join(&self.scope).join(uid).join(branch)
    }
}

pub(crate) fn scope_of(who: &Option<Principal>) -> String {
    who.as_ref().map(|p| format!("{}/", p.account().uid)).unwrap_or_default()
}

/// Checks the caller may do `verb` on the project's branch and answers
/// with the tenant to look deployments up in. In single-tenant mode
/// everything is allowed.
pub(crate) fn gate(app: &App, who: &Option<Principal>, uid: &str, branch: &str, verb: Verb) -> Result<Tenant, ApiError> {
    check_uid(uid)?;
    // Project-level routes come through with no branch at all.
    if !branch.is_empty() {
        check_branch(branch)?;
    }
    let scope = scope_of(who);
    let (store, p) = match (&app.accounts, who) {
        (None, _) => return Ok(Tenant { scope, project: None }),
        (Some(_), None) => return Err(key_required()),
        (Some(store), Some(p)) => (store, p),
    };
    let project = store.project(p.account().id, uid).map_err(internal)?.ok_or_else(|| not_found(uid, branch))?;
    store.authorize(p, &project, branch, verb).map_err(accounts_error)?;
    Ok(Tenant { scope, project: Some(project) })
}

/// The security context a plan runs under.
///
/// A query key whose grant on this project carries a context bound by
/// the project's owner plans as that context and nothing else. A
/// context in the request is refused then, even an identical one, so
/// there is never a question of which won. Everyone else plans as the
/// context they sent, as before: an application acting for many users
/// sends each one's.
pub(crate) fn effective_context(
    t: &Tenant,
    who: &Option<Principal>,
    sent: Option<SecurityContext>,
) -> Result<Option<SecurityContext>, ApiError> {
    let (Some(Principal::QueryKey { grants, .. }), Some(project)) = (who, &t.project) else { return Ok(sent) };
    let Some(bound) = grants.iter().find(|g| g.project_id == project.id).and_then(|g| g.context.as_ref()) else { return Ok(sent) };
    if sent.is_some() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "ContextBound",
            format!("this key's context on {} is bound by the project's owner; do not send one", project.uid),
        ));
    }
    let ctx: SecurityContext = serde_json::from_value(bound.clone()).map_err(|e| {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Accounts", format!("the context bound on {} does not parse: {e}", project.uid))
    })?;
    Ok(Some(ctx))
}

/// The longest project uid or branch name accepted. Both become one
/// directory name, and the limit keeps them well inside what any
/// filesystem allows.
const NAME_LIMIT: usize = 100;

/// A project uid as the URL carries it must be exactly the slug the
/// account store would keep (lower-case letters, digits, `-`, `_`),
/// because it names a directory under the account's slice of the data
/// directory and a key in the models map. The store would find
/// `Customer-Service` by lower-casing it, but the directory and the key
/// would not be the canonical ones.
pub(crate) fn check_uid(uid: &str) -> Result<(), ApiError> {
    let ok = !uid.is_empty()
        && uid.len() <= NAME_LIMIT
        && uid.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Invalid",
            format!("'{uid}' is not a project uid (lower-case letters, digits, - and _, at most {NAME_LIMIT} characters)"),
        ))
    }
}

/// A branch names a directory under the project's, and is read back
/// from that directory's name on restart, so it is one path component:
/// letters, digits, `-`, `_` and `.`, not starting with a dot. The
/// router decodes `%2F` and `%2E` before the handler sees the name, so
/// `..%2F..%2Fother-account%2Fproject%2Fmain` would otherwise reach
/// another account's deployment on disk.
pub(crate) fn check_branch(branch: &str) -> Result<(), ApiError> {
    let ok = !branch.is_empty()
        && branch.len() <= NAME_LIMIT
        && !branch.starts_with('.')
        && branch.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if ok {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Invalid",
            format!("'{branch}' is not a branch name this server accepts (letters, digits, - _ and ., not starting with a dot, at most {NAME_LIMIT} characters; a git branch with a / in its name cannot be deployed yet)"),
        ))
    }
}

pub(crate) fn model(app: &App, t: &Tenant, uid: &str, branch: &str) -> Result<Arc<Model>, ApiError> {
    read(&app.models).get(&t.key(uid, branch)).cloned().ok_or_else(|| not_found(uid, branch))
}

/// The deployments in one account's slice of the models map: the keys
/// that start with its scope (`acme/`), or every key in single-tenant
/// mode, where the scope is empty.
///
/// The map holds every account's deployments side by side, and
/// `Model` names its project and branch but not its account, so two
/// accounts that both deploy `customer-service/main` are told apart by
/// the key alone. Listing by project and branch without the key showed
/// each of them the other's deployment as a second copy of their own.
pub(crate) fn in_scope<'a, V>(models: &'a HashMap<String, V>, scope: &str) -> impl Iterator<Item = &'a V> {
    let scope = scope.to_string();
    models.iter().filter(move |(k, _)| k.starts_with(&scope)).map(|(_, d)| d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both names become directory names under the account's slice of
    /// the data directory, and the router hands them over decoded, so
    /// anything that could step out of that slice is refused before it
    /// is used. The uid must also be the canonical slug, or the same
    /// project gets a second directory and key per spelling.
    #[test]
    fn project_and_branch_names_stay_inside_the_account() {
        for uid in ["customer-service", "tpcds", "bird_financial", "a1"] {
            assert!(check_uid(uid).is_ok(), "{uid}");
        }
        for uid in [
            "",
            "Customer-Service",
            " customer-service",
            "../newco/customer-service",
            "a/b",
            "a\\b",
            "a.b",
            "a b",
            &"x".repeat(NAME_LIMIT + 1),
        ] {
            assert!(check_uid(uid).is_err(), "{uid:?}");
        }
        for branch in ["main", "master", "probe-holder-loan", "release_1.2", "v1.2.3", "RC1"] {
            assert!(check_branch(branch).is_ok(), "{branch}");
        }
        for branch in [
            "",
            ".",
            "..",
            ".hidden",
            "../../newco/customer-service/main",
            "feature/x",
            "a\\b",
            "a b",
            "a\0b",
            "main\n",
            &"x".repeat(NAME_LIMIT + 1),
        ] {
            assert!(check_branch(branch).is_err(), "{branch:?}");
        }
    }

    /// Two accounts may each deploy a project called `customer-service`
    /// on `main`; the listing must show each only its own. The scope
    /// ends in a slash, so `acme` does not reach into `acme-2` either.
    #[test]
    fn the_listing_shows_an_account_only_its_own_deployments() {
        let models: HashMap<String, &str> = [
            ("acme/customer-service/main", "acme's"),
            ("acme/web/main", "acme's web"),
            ("acme-2/customer-service/main", "acme-2's"),
            ("newco/customer-service/main", "newco's"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let seen = |scope: &str| {
            let mut v: Vec<&str> = in_scope(&models, scope).copied().collect();
            v.sort();
            v
        };
        assert_eq!(seen("acme/"), ["acme's", "acme's web"]);
        assert_eq!(seen("acme-2/"), ["acme-2's"]);
        assert_eq!(seen("newco/"), ["newco's"]);
        assert_eq!(seen("nobody/"), Vec::<&str>::new());
        // Single-tenant: no scope, everything is in scope.
        assert_eq!(seen("").len(), 4);
    }
}

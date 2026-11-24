//! The names that become directories: an org, a project's uid and a
//! branch. Each arrives in a URL the router has already decoded, so
//! each is checked before it is used for anything.

use crate::error::ApiError;
use axum::http::StatusCode;

/// The longest project uid or branch name accepted. Both become one
/// directory name, and the limit keeps them well inside what any
/// filesystem allows.
const NAME_LIMIT: usize = 100;

/// The org a request with no org in its path is for, and the directory
/// its projects are under.
pub(crate) const DEFAULT_ORG: &str = zsql_exec::DEFAULT_ORG;

/// Whether a name is a slug: lower-case letters, digits, `-` and `_`.
pub(crate) fn is_slug(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_LIMIT
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// An org names a directory under the data directory and the first
/// part of a key in the models map, so it is a slug as a project's uid
/// is. It never starts with a dot, which keeps the server's own
/// directories there apart from an org's.
pub(crate) fn check_org(org: &str) -> Result<(), ApiError> {
    if is_slug(org) {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Invalid",
            format!("'{org}' is not an org (lower-case letters, digits, - and _, at most {NAME_LIMIT} characters)"),
        ))
    }
}

/// A project uid as the URL carries it must be a slug (lower-case
/// letters, digits, `-`, `_`), because it names a directory under its
/// org's and part of a key in the models map. One spelling per
/// project, so there is one directory and one key per project.
pub(crate) fn check_uid(uid: &str) -> Result<(), ApiError> {
    if is_slug(uid) {
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
/// `..%2F..%2Fother%2Fmain` would otherwise reach another project's
/// deployment on disk, or step out of the data directory altogether.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Both names become directory names under the data directory, and
    /// the router hands them over decoded, so anything that could step
    /// out of it is refused before it is used. The uid must also be the
    /// canonical slug, or the same project gets a second directory and
    /// key per spelling.
    #[test]
    fn project_and_branch_names_stay_inside_the_data_directory() {
        for uid in ["customer-service", "tpcds", "bird_financial", "a1"] {
            assert!(check_uid(uid).is_ok(), "{uid}");
        }
        for uid in [
            "",
            "Customer-Service",
            " customer-service",
            "../other/customer-service",
            "a/b",
            "a\\b",
            "a.b",
            "a b",
            &"x".repeat(NAME_LIMIT + 1),
        ] {
            assert!(check_uid(uid).is_err(), "{uid:?}");
        }
        for org in ["default", "acme", "acme_eu-1"] {
            assert!(check_org(org).is_ok(), "{org}");
        }
        for org in ["", "Acme", ".cache", "..", "a/b", "a.b", "a b", &"x".repeat(NAME_LIMIT + 1)] {
            assert!(check_org(org).is_err(), "{org:?}");
        }
        for branch in ["main", "master", "probe-holder-loan", "release_1.2", "v1.2.3", "RC1"] {
            assert!(check_branch(branch).is_ok(), "{branch}");
        }
        for branch in
            ["", ".", "..", ".hidden", "../../other/main", "feature/x", "a\\b", "a b", "a\0b", "main\n", &"x".repeat(NAME_LIMIT + 1)]
        {
            assert!(check_branch(branch).is_err(), "{branch:?}");
        }
    }
}

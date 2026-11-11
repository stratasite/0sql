//! The names that become directories: a project's uid and a branch.
//! Both arrive in a URL the router has already decoded, so both are
//! checked before they are used for anything.

use crate::error::ApiError;
use axum::http::StatusCode;

/// The longest project uid or branch name accepted. Both become one
/// directory name, and the limit keeps them well inside what any
/// filesystem allows.
const NAME_LIMIT: usize = 100;

/// A project uid as the URL carries it must be a slug (lower-case
/// letters, digits, `-`, `_`), because it names a directory under the
/// data directory and half of a key in the models map. One spelling per
/// project, so there is one directory and one key per project.
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

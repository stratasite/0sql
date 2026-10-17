//! One error type for the API: a status, a class the goldens would
//! name, and a message, and the conversions into it.

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::de::DeserializeOwned;
use serde_json::json;
use zsql_service::{Kind, ServiceError};

/// An error as the API reports it: a status, a class the goldens would
/// name, and a message.
pub(crate) struct ApiError(pub(crate) StatusCode, pub(crate) &'static str, pub(crate) String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": { "class": self.1, "message": self.2 } }))).into_response()
    }
}

/// A refusal from the service, as the API reports it.
impl From<ServiceError> for ApiError {
    fn from(e: ServiceError) -> Self {
        let status = match e.kind {
            Kind::Invalid => StatusCode::BAD_REQUEST,
            Kind::Unprocessable => StatusCode::UNPROCESSABLE_ENTITY,
            Kind::NotFound => StatusCode::NOT_FOUND,
            Kind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(status, e.class, e.message)
    }
}

/// A failure of the store itself: logged here, answered with a fixed
/// line, since what SQLite has to say is about this box.
pub(crate) fn internal(e: zsql_accounts::Error) -> ApiError {
    say!("zsqld: accounts: {e}");
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Accounts", "the accounts store failed; see the server's log".into())
}

pub(crate) fn accounts_error(e: zsql_accounts::Error) -> ApiError {
    match e {
        zsql_accounts::Error::NotFound(m) => ApiError(StatusCode::NOT_FOUND, "NotFound", m),
        zsql_accounts::Error::Denied(m) => ApiError(StatusCode::FORBIDDEN, "Forbidden", m),
        zsql_accounts::Error::Invalid(m) => ApiError(StatusCode::BAD_REQUEST, "Invalid", m),
        zsql_accounts::Error::Db(e) => internal(zsql_accounts::Error::Db(e)),
    }
}

/// With accounts on, every read and write names a key; the one caller
/// without one, the MCP handshake, is answered by `mcp.rs` before it
/// reaches a gate.
pub(crate) fn key_required() -> ApiError {
    ApiError(StatusCode::UNAUTHORIZED, "Unauthorized", "an API key is required: Authorization: Bearer <key>".into())
}

pub(crate) fn not_found(uid: &str, branch: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "NotFound", format!("no deployment for project {uid} branch {branch}"))
}

/// An answer that says when to come back.
pub(crate) fn retry_later(status: StatusCode, class: &'static str, message: String, secs: u64) -> Response {
    let mut res = ApiError(status, class, message).into_response();
    if secs > 0 {
        if let Ok(v) = secs.to_string().parse() {
            res.headers_mut().insert("retry-after", v);
        }
    }
    res
}

// The extractors below are axum's, with the refusal in the API's own
// shape. axum's rejections are plain text, and a client that reads
// `error.class` from every other answer would get nothing from them.

/// A JSON body.
pub(crate) struct Input<T>(pub(crate) T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Input<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(Input(v)),
            Err(e) => Err(ApiError(e.status(), "Invalid", e.body_text())),
        }
    }
}

/// The path's parameters.
pub(crate) struct Params<T>(pub(crate) T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for Params<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(v)) => Ok(Params(v)),
            Err(e) => Err(ApiError(e.status(), "Invalid", e.body_text())),
        }
    }
}

/// The query string.
pub(crate) struct Search<T>(pub(crate) T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Search<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(v)) => Ok(Search(v)),
            Err(e) => Err(ApiError(e.status(), "Invalid", e.body_text())),
        }
    }
}

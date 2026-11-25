//! One error type for the API: a status, a class the goldens would
//! name, and a message, and the conversions into it.

use crate::names::{check_org, DEFAULT_ORG};
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::de::DeserializeOwned;
use serde_json::json;
use zsql_core::Kind;

/// An error as the API reports it: a status, a class the goldens would
/// name, and a message.
pub(crate) struct ApiError(pub(crate) StatusCode, pub(crate) &'static str, pub(crate) String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": { "class": self.1, "message": self.2 } }))).into_response()
    }
}

/// A refusal from the service, as the API reports it.
impl From<zsql_core::Error> for ApiError {
    fn from(e: zsql_core::Error) -> Self {
        let status = match e.kind {
            Kind::Invalid => StatusCode::BAD_REQUEST,
            Kind::Unprocessable => StatusCode::UNPROCESSABLE_ENTITY,
            Kind::NotFound => StatusCode::NOT_FOUND,
            Kind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(status, e.class, e.message)
    }
}

pub(crate) fn not_found(org: &str, uid: &str, branch: &str) -> ApiError {
    let within = if org == DEFAULT_ORG { String::new() } else { format!(" in org {org}") };
    ApiError(StatusCode::NOT_FOUND, "NotFound", format!("no deployment for project {uid} branch {branch}{within}"))
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

/// What the org is called in a route: `/orgs/{org}/...`.
const ORG: &str = "org";

async fn raw_params<S: Send + Sync>(parts: &mut Parts, state: &S) -> Result<axum::extract::RawPathParams, ApiError> {
    axum::extract::RawPathParams::from_request_parts(parts, state).await.map_err(|e| ApiError(e.status(), "Invalid", e.body_text()))
}

/// The path's parameters below the org, in order, as a tuple. Every
/// route is served twice, with an org in front and without, and a
/// handler reads the same parameters either way; the org is [`Org`]'s.
pub(crate) struct Params<T>(pub(crate) T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for Params<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let raw = raw_params(parts, state).await?;
        let values: Vec<String> = raw.iter().filter(|(name, _)| *name != ORG).map(|(_, value)| value.to_string()).collect();
        T::deserialize(serde::de::value::SeqDeserializer::<_, serde::de::value::Error>::new(values.into_iter()))
            .map(Params)
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, "Invalid", e.to_string()))
    }
}

/// The org a request is for: the one its path names, checked, or the
/// default one when the path names none.
pub(crate) struct Org(pub(crate) String);

impl<S: Send + Sync> FromRequestParts<S> for Org {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let raw = raw_params(parts, state).await?;
        match raw.iter().find(|(name, _)| *name == ORG) {
            Some((_, org)) => {
                check_org(org)?;
                Ok(Org(org.to_string()))
            }
            None => Ok(Org(DEFAULT_ORG.to_string())),
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

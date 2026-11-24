//! What every request meets before its handler: the admission cap on
//! its kind of work, and its deadline.
//!
//! There is no authentication here, on any route, a deploy included. The server answers whoever
//! can reach it, the way a database on a private network does: who may
//! ask, as whom, and how often belongs to the service in front of it.

use crate::admission::{self, Class};
use crate::error::retry_later;
use crate::Shared;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use std::sync::Arc;
use tokio::sync::OwnedSemaphorePermit;

/// The admission permit, carried on the request. The guard holds one
/// clone until it answers; a handler that does its work on the blocking
/// pool moves another into that work, so the permit is released when
/// the work ends and not when the deadline gives up on it. Otherwise
/// deploys that outlive their deadline would pile up past the cap.
#[derive(Clone)]
pub(crate) struct Held(#[allow(dead_code)] pub(crate) Arc<Option<OwnedSemaphorePermit>>);

pub(crate) async fn guard(State(app): State<Shared>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let class = admission::classify(req.uri().path());
    if class == Class::Exempt {
        return next.run(req).await;
    }
    // Room to run, or come back later. The permit is held until the
    // handler has finished with it.
    let held = match app.admission.admit(class).await {
        Ok(p) => Held(Arc::new(p)),
        Err(secs) => return busy(secs, class),
    };
    let (mut parts, body) = req.into_parts();
    parts.extensions.insert(held.clone());
    let req = axum::extract::Request::from_parts(parts, body);
    let response = match tokio::time::timeout(admission::deadline(class), next.run(req)).await {
        Ok(r) => r,
        Err(_) => retry_later(
            StatusCode::SERVICE_UNAVAILABLE,
            "Timeout",
            format!("this request passed its {}s deadline", admission::deadline(class).as_secs()),
            0,
        ),
    };
    drop(held);
    response
}

pub(crate) fn busy(secs: u64, class: Class) -> Response {
    retry_later(StatusCode::SERVICE_UNAVAILABLE, "Busy", format!("the server is full of {} work: retry in {secs}s", class.name()), secs)
}

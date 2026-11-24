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
pub(crate) struct Held(Arc<std::sync::Mutex<Option<OwnedSemaphorePermit>>>);

impl Held {
    pub(crate) fn new(permit: Option<OwnedSemaphorePermit>) -> Held {
        Held(Arc::new(std::sync::Mutex::new(permit)))
    }

    /// Gives the seat up now, whoever else still holds this.
    fn release(&self) {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
    }
}

tokio::task_local! {
    /// The seat the request in hand was admitted on.
    static SEAT: Held;
}

/// Gives up the seat this request was admitted on, before it is
/// answered: for a handler that has found its work is of another class
/// and has taken a seat there, so a long wait does not hold a seat
/// meant for quick work.
pub(crate) fn release_seat() {
    let _ = SEAT.try_with(Held::release);
}

pub(crate) async fn guard(State(app): State<Shared>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let class = admission::classify(req.uri().path());
    if class == Class::Exempt {
        return next.run(req).await;
    }
    // Room to run, or come back later. The permit is held until the
    // handler has finished with it.
    let held = match app.admission.admit(class).await {
        Ok(p) => Held::new(p),
        Err(secs) => return busy(secs, class),
    };
    let (mut parts, body) = req.into_parts();
    parts.extensions.insert(held.clone());
    let req = axum::extract::Request::from_parts(parts, body);
    let response = match tokio::time::timeout(admission::deadline(class), SEAT.scope(held.clone(), next.run(req))).await {
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

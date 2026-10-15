//! The web app, embedded: the shell, its script and stylesheet, and
//! the brand files browsers expect at the site root.

use crate::Shared;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};

/// The app shell and its assets, embedded. Every `/app/...` path gets the
/// shell; the script reads the path and renders the view.
pub(crate) async fn ui(Path(rest): Path<String>) -> Response {
    ui_file(&rest)
}

pub(crate) async fn ui_root() -> Response {
    ui_file("")
}

/// The files browsers and manifests expect at the site root: favicons,
/// the touch icon, the Android icons and the manifest that names them.
pub(crate) const SITE_FILES: &[&str] = &[
    "favicon.ico",
    "favicon.svg",
    "apple-touch-icon.png",
    "site.webmanifest",
    "icon-192.png",
    "icon-512.png",
    "icon-maskable-192.png",
    "icon-maskable-512.png",
];

pub(crate) async fn site_file(uri: axum::http::Uri) -> Response {
    ui_file(uri.path().trim_start_matches('/'))
}

/// An embedded asset: its bytes, media type, and whether browsers may keep
/// it for a day (the brand files never change under one build; the shell,
/// stylesheet and script are revalidated every time).
fn asset(path: &str) -> Option<(&'static [u8], &'static str, bool)> {
    Some(match path {
        "app.css" => (include_bytes!("../ui/app.css"), "text/css; charset=utf-8", false),
        "app.js" => (include_bytes!("../ui/app.js"), "text/javascript; charset=utf-8", false),
        "brand/wordmark-light.svg" => (include_bytes!("../ui/brand/wordmark-light.svg"), "image/svg+xml", true),
        "brand/wordmark-dark.svg" => (include_bytes!("../ui/brand/wordmark-dark.svg"), "image/svg+xml", true),
        "brand/icon-light.svg" => (include_bytes!("../ui/brand/icon-light.svg"), "image/svg+xml", true),
        "brand/icon-dark.svg" => (include_bytes!("../ui/brand/icon-dark.svg"), "image/svg+xml", true),
        "favicon.svg" => (include_bytes!("../ui/brand/favicon.svg"), "image/svg+xml", true),
        "favicon.ico" => (include_bytes!("../ui/brand/favicon.ico"), "image/x-icon", true),
        "apple-touch-icon.png" => (include_bytes!("../ui/brand/apple-touch-icon.png"), "image/png", true),
        "icon-192.png" => (include_bytes!("../ui/brand/icon-192.png"), "image/png", true),
        "icon-512.png" => (include_bytes!("../ui/brand/icon-512.png"), "image/png", true),
        "icon-maskable-192.png" => (include_bytes!("../ui/brand/icon-maskable-192.png"), "image/png", true),
        "icon-maskable-512.png" => (include_bytes!("../ui/brand/icon-maskable-512.png"), "image/png", true),
        "site.webmanifest" => (include_bytes!("../ui/brand/site.webmanifest"), "application/manifest+json", true),
        _ => return None,
    })
}

fn ui_file(path: &str) -> Response {
    let (body, mime, cache): (&'static [u8], &str, &str) = match asset(path) {
        Some((body, mime, stable)) => (body, mime, if stable { "public, max-age=86400" } else { "no-cache" }),
        None => (include_bytes!("../ui/app.html"), "text/html; charset=utf-8", "no-cache"),
    };
    let mut r = ([("content-type", mime.to_string()), ("cache-control", cache.to_string())], body).into_response();
    let h = r.headers_mut();
    h.insert("x-content-type-options", "nosniff".parse().unwrap());
    h.insert("referrer-policy", "same-origin".parse().unwrap());
    h.insert("x-frame-options", "DENY".parse().unwrap());
    if mime.starts_with("text/html") {
        // Every interpolation in the app escapes, so this is the net
        // under that: a script or style from anywhere but here does not
        // run, and the page cannot be framed.
        h.insert("content-security-policy", UI_CSP.parse().unwrap());
    }
    r
}

const UI_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'";

pub(crate) async fn root(State(app): State<Shared>) -> Response {
    if app.accounts.is_some() {
        axum::response::Redirect::to("/app/").into_response()
    } else {
        "zsqld: accounts off; see /healthz".into_response()
    }
}

pub(crate) async fn healthz() -> &'static str {
    "ok"
}

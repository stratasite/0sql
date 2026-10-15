//! Local and server answer alike.
//!
//! `zsql` plans from a project directory through `zsql_service::Model`;
//! `zsqld` plans from the same files after a deploy, through the same
//! type, behind the gate. These tests hold the two to byte-identical
//! answers over the fixture projects: the SQL, the explain graph, the
//! catalogue, the field search, and the refusals with their classes.
//! A difference here is a seam the service crate was meant to close.

use super::*;
use axum::body::Body;
use axum::http::{Method, Request};
use tower::ServiceExt;
use zsql_service::unlimited;

const PEER: SocketAddr = SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 40001);

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/projects")
}

/// A single-tenant server over a scratch data directory: no accounts,
/// no limits, as a developer's own `zsqld` runs.
fn single_tenant(data_dir: &std::path::Path) -> Shared {
    let dialects: &'static Dialects = Box::leak(Box::new(Dialects::builtin()));
    Arc::new(App {
        dialects,
        data_dir: data_dir.to_path_buf(),
        models: RwLock::new(HashMap::new()),
        accounts: None,
        signup_open: false,
        secure_cookies: false,
        limits: Limiter::new(limits::Config { mode: limits::Mode::Off, ..limits::Config::default() }),
        admission: Admission::from_env().unwrap(),
        caps: Caps { per_project: usize::MAX, per_account: usize::MAX },
        mailer: mail::Mailer::from_env("http://test".into()).unwrap(),
    })
}

/// The project directory as the CLI would send it: every file under
/// it, at its relative path.
fn archive_of_dir(dir: &std::path::Path) -> Vec<u8> {
    fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
        entries.sort_by_key(|e| e.path());
        for e in entries {
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.push((p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"), std::fs::read(&p).unwrap()));
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, dir, &mut files);
    let mut buf = Vec::new();
    {
        let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::fast());
        let mut tar = tar::Builder::new(enc);
        for (path, bytes) in &files {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, path, bytes.as_slice()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
    }
    buf
}

async fn call(router: &Router, method: Method, path: &str, body: Option<Vec<u8>>, content_type: &str) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path).extension(ConnectInfo(PEER));
    if body.is_some() {
        req = req.header("content-type", content_type);
    }
    let req = req.body(Body::from(body.unwrap_or_default())).unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn post_json(router: &Router, path: &str, body: &Value) -> (StatusCode, Value) {
    call(router, Method::POST, path, Some(serde_json::to_vec(body).unwrap()), "application/json").await
}

async fn get(router: &Router, path: &str) -> (StatusCode, Value) {
    call(router, Method::GET, path, None, "").await
}

/// What the service says, as the server would report it.
fn as_server(r: Result<Value, zsql_service::ServiceError>) -> (StatusCode, Value) {
    match r {
        Ok(v) => (StatusCode::OK, v),
        Err(e) => {
            let ApiError(status, class, message) = ApiError::from(e);
            (status, json!({ "error": { "class": class, "message": message } }))
        }
    }
}

/// Specs worth comparing over a model: each visible measure alone, each
/// of the first dimensions with each of the first measures, a filter, a
/// near-miss name and an unknown one.
fn specs_over(model: &Model) -> Vec<Value> {
    let u = model.universe();
    let names = |kind: zsql_semantic::FieldKind, n: usize| -> Vec<String> {
        u.fields().iter().filter(|f| f.kind == kind && !f.hidden).map(|f| f.name.clone()).take(n).collect()
    };
    let dims = names(zsql_semantic::FieldKind::Dimension, 6);
    let msrs = names(zsql_semantic::FieldKind::Measure, 6);
    let proj = |fields: &[&str]| json!({ "projections": fields.iter().map(|f| json!({ "field": f })).collect::<Vec<_>>() });
    let mut out: Vec<Value> = Vec::new();
    for m in names(zsql_semantic::FieldKind::Measure, 40) {
        out.push(proj(&[&m]));
    }
    for d in &dims {
        for m in &msrs {
            out.push(proj(&[d, m]));
        }
    }
    if let (Some(d), Some(m)) = (dims.first(), msrs.first()) {
        out.push(json!({ "projections": [{ "field": d }, { "field": m }], "filters": [{ "field": d, "predicate": "is_not_null" }], "limit": 10 }));
        // A near miss resolves with a correction; nonsense refuses.
        let near: String = d.chars().take(d.len().saturating_sub(1).max(3)).collect();
        out.push(proj(&[&near, m]));
    }
    out.push(proj(&["no such field anywhere"]));
    out
}

#[tokio::test]
async fn local_and_server_answer_alike() {
    let root = fixtures_root();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("fixtures/projects")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("project.yml").exists())
        .collect();
    dirs.sort();
    assert!(!dirs.is_empty(), "no fixture projects under {}", root.display());
    let data_dir = std::env::temp_dir().join(format!("zsqld-parity-{}", std::process::id()));
    let app = single_tenant(&data_dir);
    let router = router(app.clone());
    let dialects: &'static Dialects = Box::leak(Box::new(Dialects::builtin()));
    let mut compared = 0;
    for dir in &dirs {
        let local = Model::from_dir(dir, Trust::Owned, dialects).expect("the fixture loads");
        let uid = local.uid.clone();
        let base = format!("/projects/{uid}/branches/main");
        let (status, body) = call(&router, Method::POST, &format!("{base}/deploy"), Some(archive_of_dir(dir)), "application/gzip").await;
        assert_eq!(status, StatusCode::OK, "{uid}: {body}");
        assert_eq!(body["test_results"], local.run_tests(), "{uid}: test results");
        assert_eq!(body["fields"], local.summary()["fields"], "{uid}: field count");

        for spec in specs_over(&local) {
            let req = || zsql_service::SqlRequest { spec: Some(spec.clone()), expr: None, context: None };
            let server = post_json(&router, &format!("{base}/sql"), &json!({ "spec": spec })).await;
            let here = as_server(local.sql(req(), &unlimited).map(|r| serde_json::to_value(r).unwrap()));
            assert_eq!(server, here, "{uid} sql over {spec}");
            let (ss, sv) = post_json(&router, &format!("{base}/explain"), &json!({ "spec": spec })).await;
            let (hs, hv) = as_server(local.explain(req(), &unlimited).map(|r| serde_json::to_value(r).unwrap()));
            assert_eq!(ss, hs, "{uid} explain status over {spec}");
            // Timings differ by nature; everything else must not.
            for key in ["sql", "datasource", "adapter", "nodes", "corrections", "error"] {
                assert_eq!(sv[key], hv[key], "{uid} explain {key} over {spec}");
            }
            assert_eq!(
                sv["phases"].as_array().map(|p| p.iter().map(|x| x["name"].clone()).collect::<Vec<_>>()),
                hv["phases"].as_array().map(|p| p.iter().map(|x| x["name"].clone()).collect::<Vec<_>>()),
                "{uid} explain phases over {spec}"
            );
            let (ss, sv) = post_json(&router, &format!("{base}/explore"), &json!({ "spec": spec, "q": "a" })).await;
            let (hs, hv) = as_server(
                local
                    .explore(
                        zsql_service::ExploreRequest { spec: Some(spec.clone()), expr: None, context: None, q: Some("a".into()) },
                        &unlimited,
                    )
                    .map(|r| serde_json::to_value(r).unwrap()),
            );
            assert_eq!(ss, hs, "{uid} explore status over {spec}");
            for key in ["dimensions", "measures", "error"] {
                assert_eq!(sv[key], hv[key], "{uid} explore {key} over {spec}");
            }
            compared += 1;
        }

        // The shorthand, read by whoever plans it.
        for line in [
            "sales, quantity",
            "department, sales value, department = PRODUCE",
            "item brand id, store quantity desc",
            "nothing here at all",
        ] {
            let server = post_json(&router, &format!("{base}/sql"), &json!({ "expr": line })).await;
            let here = as_server(
                local
                    .sql(zsql_service::SqlRequest { spec: None, expr: Some(line.into()), context: None }, &unlimited)
                    .map(|r| serde_json::to_value(r).unwrap()),
            );
            assert_eq!(server, here, "{uid} expr {line}");
        }

        // The catalogue and the search.
        assert_eq!(get(&router, &format!("{base}/tables")).await, (StatusCode::OK, local.tables()), "{uid} tables");
        for (q, hidden) in [("", false), ("", true), ("date", false), ("disc", true), ("zzzz", false)] {
            assert_eq!(
                get(&router, &format!("{base}/fields?q={q}&hidden={hidden}")).await,
                (StatusCode::OK, local.fields(q, hidden)),
                "{uid} fields {q:?} hidden {hidden}"
            );
        }
        assert_eq!(get(&router, &format!("{base}/values/department")).await, as_server(local.values_of("department")), "{uid} values");
    }
    assert!(compared > 40, "only {compared} specs compared");
    let _ = std::fs::remove_dir_all(&data_dir);
}

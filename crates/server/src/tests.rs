//! The server, end to end, through the real router: the guard, the
//! names that become directories, the archive, the
//! models map, the data directory, and the MCP endpoint.
//!
//! Each test gets its own server over a scratch data directory. The
//! fixture project is small: one table, two dimensions, one measure,
//! one planner test, and a marker dimension named for the deploy, so
//! that which model answers can be read off `/fields`.

use crate::admission::{Admission, AdmissionCounts};
use crate::deploy::{nanos, Capped, UNPACKED_LIMIT};
use crate::mcp;
use crate::run::{reload, router};
use crate::{App, Shared};
use axum::body::Body;
use axum::http::StatusCode;
use axum::http::{Method, Request};
use axum::Router;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tower::ServiceExt;
use zsql_dialect::Dialects;

struct Server {
    app: Shared,
    router: Router,
    data_dir: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

/// A server over a scratch data directory: every route is open, as it always is.
fn server() -> Server {
    let data_dir = std::env::temp_dir().join(format!("zsqld-tests-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&data_dir).unwrap();
    let app = app_over(&data_dir);
    Server { router: router(app.clone()), app, data_dir }
}

fn app_over(data_dir: &std::path::Path) -> Shared {
    let dialects: &'static Dialects = Box::leak(Box::new(Dialects::builtin()));
    Arc::new(App {
        dialects,
        data_dir: data_dir.to_path_buf(),
        models: RwLock::new(HashMap::new()),
        admission: Admission::new(&AdmissionCounts::default()),
        mcp: mcp::Settings::builtin(),
        deploying: std::sync::Mutex::new(()),
        runs: crate::open_runs(data_dir, &crate::RunLimits::default()).unwrap(),
        default_sql_limit: None,
    })
}

/// A status and a JSON body (null when the body is not JSON).
#[derive(Debug)]
struct Answer {
    status: StatusCode,
    body: Value,
    headers: axum::http::HeaderMap,
    /// The body as it came, for an answer that is not JSON.
    raw: Vec<u8>,
}

impl Answer {
    fn error(&self) -> String {
        self.body["error"]["message"].as_str().unwrap_or("").to_string()
    }
    fn class(&self) -> &str {
        self.body["error"]["class"].as_str().unwrap_or("")
    }
}

impl Server {
    async fn send(&self, method: Method, path: &str, headers: &[(&str, &str)], body: Option<(&str, Vec<u8>)>) -> Answer {
        let mut req = Request::builder().method(method).uri(path);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let body = match body {
            Some((ct, bytes)) => {
                req = req.header("content-type", ct);
                Body::from(bytes)
            }
            None => Body::empty(),
        };
        let res = self.router.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), 16 << 20).await.unwrap();
        Answer { status, body: serde_json::from_slice(&bytes).unwrap_or(Value::Null), headers, raw: bytes.to_vec() }
    }

    /// One JSON-RPC message to the MCP endpoint, with any headers.
    async fn mcp(&self, headers: &[(&str, &str)], msg: Value) -> Answer {
        self.send(Method::POST, "/mcp", headers, Some(("application/json", serde_json::to_vec(&msg).unwrap()))).await
    }

    /// A tool call; answers the JSON-RPC envelope.
    async fn tool(&self, headers: &[(&str, &str)], name: &str, args: Value) -> Answer {
        self.mcp(headers, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } })).await
    }

    async fn get(&self, path: &str) -> Answer {
        self.send(Method::GET, path, &[], None).await
    }
    async fn post(&self, path: &str, json: Value) -> Answer {
        self.send(Method::POST, path, &[], Some(("application/json", serde_json::to_vec(&json).unwrap()))).await
    }
    async fn deploy(&self, uid: &str, branch: &str, archive: Vec<u8>) -> Answer {
        self.send(Method::POST, &format!("/projects/{uid}/branches/{branch}/deploy"), &[], Some(("application/gzip", archive))).await
    }
    async fn validate(&self, uid: &str, branch: &str, archive: Vec<u8>) -> Answer {
        self.send(Method::POST, &format!("/projects/{uid}/branches/{branch}/validate"), &[], Some(("application/gzip", archive))).await
    }

    /// The field names of a branch, or the error.
    async fn fields(&self, uid: &str, branch: &str) -> Result<Vec<String>, Answer> {
        let a = self.get(&format!("/projects/{uid}/branches/{branch}/fields?hidden=true")).await;
        if a.status != StatusCode::OK {
            return Err(a);
        }
        Ok(a.body["fields"].as_array().unwrap().iter().filter_map(|f| f["name"].as_str().map(str::to_string)).collect())
    }

    /// What the listing holds, as `project/branch`.
    async fn listing(&self) -> Vec<String> {
        let a = self.get("/projects").await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.error());
        a.body["deployments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| format!("{}/{}", d["project"].as_str().unwrap(), d["branch"].as_str().unwrap()))
            .collect()
    }

    /// Deployment directories on disk, relative to the data directory:
    /// `org/project/branch`.
    fn on_disk(&self) -> Vec<String> {
        let dirs = |p: &std::path::Path| -> Vec<PathBuf> {
            std::fs::read_dir(p).unwrap().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()
        };
        let name = |p: &std::path::Path| p.file_name().unwrap().to_string_lossy().to_string();
        let mut out = Vec::new();
        for org in dirs(&self.data_dir) {
            for project in dirs(&org) {
                for branch in dirs(&project) {
                    if branch.join("snapshot.json.gz").exists() {
                        out.push(format!("{}/{}/{}", name(&org), name(&project), name(&branch)));
                    }
                }
            }
        }
        out.sort();
        out
    }

    /// A deploy under an org.
    async fn deploy_to(&self, org: &str, uid: &str, branch: &str, archive: Vec<u8>) -> Answer {
        self.send(Method::POST, &format!("/orgs/{org}/projects/{uid}/branches/{branch}/deploy"), &[], Some(("application/gzip", archive)))
            .await
    }

    /// A tool call on an org's MCP endpoint; answers the JSON-RPC envelope.
    async fn tool_of(&self, org: &str, name: &str, args: Value) -> Answer {
        let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } });
        self.send(Method::POST, &format!("/orgs/{org}/mcp"), &[], Some(("application/json", serde_json::to_vec(&msg).unwrap()))).await
    }

    fn keys_in_memory(&self) -> Vec<String> {
        let mut v: Vec<String> = self.app.models.read().unwrap().keys().cloned().collect();
        v.sort();
        v
    }
}

// ------------------------------------------------------------- fixtures

/// The fixture project as a tar.gz, with a marker dimension so the
/// model that answers can be told from any other.
fn archive(uid: &str, marker: &str) -> Vec<u8> {
    archive_of(&fixture_files(uid, marker))
}

fn fixture_files(uid: &str, marker: &str) -> Vec<(String, String)> {
    let column = format!("marker_{}", marker.replace('-', "_"));
    vec![
        ("project.yml".into(), format!("name: Fixture {marker}\nuid: {uid}\nproduction_branch: main\n")),
        ("datasources.yml".into(), "dw:\n  name: Warehouse\n  adapter: duckdb\n  tier: hot\n".into()),
        (
            "models/tbl.sales.yml".into(),
            format!(
                "name: Sales\nphysical_name: sales\ndatasource: dw\ncost: 10\nfields:\n\
                 \x20 - type: dimension\n    name: Category\n    data_type: string\n    expression:\n      sql: category\n\
                 \x20 - type: dimension\n    name: Marker {marker}\n    data_type: string\n    expression:\n      sql: {column}\n\
                 \x20 - type: measure\n    name: Revenue\n    data_type: decimal\n    expression:\n      sql: sum(revenue)\n"
            ),
        ),
        ("tests/smoke.yml".into(), "name: smoke\nprojections: [Category, Revenue]\nassert_regex: SELECT\n".into()),
    ]
}

/// The fixture with two row-level policies: `Revenue` is filtered by the
/// groups in the context on `Region`, and `Category` by the context's
/// email on `Owner Email`. Both deny when nothing resolves.
fn archive_with_policies(uid: &str, marker: &str) -> Vec<u8> {
    let mut files = fixture_files(uid, marker);
    let model = files.iter_mut().find(|(p, _)| p == "models/tbl.sales.yml").unwrap();
    model.1.push_str(
        "  - type: dimension\n    name: Region\n    data_type: string\n    expression:\n      sql: region\n\
         \x20 - type: dimension\n    name: Owner Email\n    data_type: string\n    expression:\n      sql: owner_email\n",
    );
    files.push((
        "security.yml".into(),
        "policies:\n\
         \x20 - name: regions\n    mode: filter_data\n    context_dimension: Region\n    triggers:\n      field_names: [Revenue]\n\
         \x20   permission_resolution:\n      source: groups\n      value_from: name\n      unresolved: deny\n\
         \x20 - name: owners\n    mode: filter_data\n    context_dimension: Owner Email\n    triggers:\n      field_names: [Category]\n\
         \x20   permission_resolution:\n      source: user\n      value_from: email\n      unresolved: deny\n"
            .into(),
    ));
    archive_of(&files)
}

/// The policy fixture plus measures whose own rules touch the context
/// dimension: one that ignores a caller's Region filter, one that
/// ignores every filter, and one computed per Region then rolled up.
/// The regions policy is triggered by the `secure` tag they all carry.
fn archive_with_exclusions(uid: &str, marker: &str) -> Vec<u8> {
    let mut files = fixture_files(uid, marker);
    let model = files.iter_mut().find(|(p, _)| p == "models/tbl.sales.yml").unwrap();
    model.1.push_str(
        "  - type: dimension\n    name: Region\n    data_type: string\n    expression:\n      sql: region\n\
         \x20 - type: measure\n    name: Total Revenue\n    data_type: decimal\n    tags: [secure]\n    exclusion_type: exclude\n    exclusions:\n      - type: dimension\n        filter: ignore\n        entities: [Region]\n    expression:\n      sql: sum(revenue)\n\
         \x20 - type: measure\n    name: Global Revenue\n    data_type: decimal\n    tags: [secure]\n    exclusion_type: exclude_all\n    expression:\n      sql: sum(revenue)\n\
         \x20 - type: measure\n    name: Peak Region Revenue\n    data_type: decimal\n    tags: [secure]\n    inclusions:\n      filter: ignore\n      aggregation: max\n      dimensions: [Region]\n    expression:\n      sql: sum(revenue)\n",
    );
    files.push((
        "security.yml".into(),
        "policies:\n\
         \x20 - name: regions\n    mode: filter_data\n    context_dimension: Region\n    triggers:\n      field_tags: [secure]\n\
         \x20   permission_resolution:\n      source: groups\n      value_from: name\n      unresolved: deny\n"
            .into(),
    ));
    archive_of(&files)
}

fn archive_of(files: &[(String, String)]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::fast());
        let mut tar = tar::Builder::new(enc);
        for (path, text) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(text.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, path, text.as_bytes()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
    }
    buf
}

/// The fixture plus a symbolic link entry pointing at `target`.
fn archive_with_link(uid: &str, link_path: &str, target: &str) -> Vec<u8> {
    let files = fixture_files(uid, "linked");
    let mut buf = Vec::new();
    {
        let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::fast());
        let mut tar = tar::Builder::new(enc);
        for (path, text) in &files {
            let mut h = tar::Header::new_gnu();
            h.set_size(text.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, path, text.as_bytes()).unwrap();
        }
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        h.set_cksum();
        tar.append_link(&mut h, link_path, target).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }
    buf
}

const CS: &str = "customer-service";
const EMEA: &str = r#"{"groups":[{"name":"emea"}]}"#;

/// The spec a shorthand line makes, for the tools, which take specs only.
fn spec_of(expr: &str) -> Value {
    zsql_shorthand::parse_line(expr).unwrap().0
}

// ---------------------------------------------------------------- tests

/// With no token the server is as open to writes as to reads: the mode
/// a developer's own machine runs in.
#[tokio::test]
async fn every_route_is_open() {
    let s = server();
    let a = s.deploy(CS, "main", archive(CS, "one")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert_eq!(s.listing().await, ["customer-service/main"]);
    assert_eq!(s.on_disk(), ["default/customer-service/main"]);
    let a = s.send(Method::POST, &format!("/projects/{CS}/branches/main/test"), &[], None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s.send(Method::DELETE, &format!("/projects/{CS}/branches/main"), &[], None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert!(s.listing().await.is_empty() && s.on_disk().is_empty());
}

/// Nothing a request carries is read as a credential: a deploy, a
/// validate, a test and a removal are answered the same with no
/// `Authorization` header, with one, and with one left over from a
/// server that used to ask for a token.
#[tokio::test]
async fn an_authorization_header_is_neither_needed_nor_read() {
    let s = server();
    let branch = format!("/projects/{CS}/branches/main");
    for sent in [None, Some("Bearer zdt_a-token-from-an-older-server"), Some("Basic bm9wZQ=="), Some("nonsense")] {
        let headers: Vec<(&str, &str)> = sent.iter().map(|v| ("authorization", *v)).collect();
        for (method, path, body) in [
            (Method::POST, format!("{branch}/validate"), Some(("application/gzip", archive(CS, "one")))),
            (Method::POST, format!("{branch}/deploy"), Some(("application/gzip", archive(CS, "one")))),
            (Method::POST, format!("{branch}/test"), None),
            (Method::DELETE, branch.clone(), None),
        ] {
            let a = s.send(method.clone(), &path, &headers, body).await;
            assert_eq!(a.status, StatusCode::OK, "{method} {path} with {sent:?}: {}", a.body);
            assert!(a.headers.get("www-authenticate").is_none(), "nothing is asked for");
        }
    }
    assert!(s.on_disk().is_empty(), "the last of them was the removal");
}

/// Two projects, and two branches of one, are each their own
/// deployment: one answers for itself and removing one leaves the rest.
#[tokio::test]
async fn each_branch_is_its_own_deployment() {
    let s = server();
    for (uid, branch, marker) in [(CS, "main", "cs-main"), (CS, "dev", "cs-dev"), ("billing", "main", "billing")] {
        let a = s.deploy(uid, branch, archive(uid, marker)).await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    }
    assert_eq!(s.listing().await, ["billing/main", "customer-service/dev", "customer-service/main"]);
    assert_eq!(s.on_disk(), s.keys_in_memory());
    assert!(s.fields(CS, "dev").await.unwrap().contains(&"Marker cs-dev".to_string()));
    assert!(s.fields(CS, "main").await.unwrap().contains(&"Marker cs-main".to_string()));
    let a = s.send(Method::DELETE, &format!("/projects/{CS}/branches/dev"), &[], None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert_eq!(s.listing().await, ["billing/main", "customer-service/main"]);
    assert_eq!(s.fields(CS, "dev").await.unwrap_err().status, StatusCode::NOT_FOUND);

    // A restart finds what is on disk and nothing else.
    let again = app_over(&s.data_dir);
    let (n, skipped) = reload(&again).unwrap();
    assert_eq!((n, skipped.len()), (2, 0));
}

/// A name that could step out of its project's directory, or out of
/// the data directory, is refused before it is used for anything.
#[tokio::test]
async fn names_that_could_leave_the_data_directory_are_refused() {
    let s = server();
    let a = s.deploy(CS, "main", archive(CS, "one")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s.deploy("billing", "main", archive("billing", "two")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let other = s.data_dir.join("default").join("billing").join("main").join("snapshot.json.gz");
    let before = std::fs::read(&other).unwrap();

    let traversal = "..%2F..%2Fbilling%2Fmain";
    for (method, path) in [
        (Method::POST, format!("/projects/{CS}/branches/{traversal}/deploy")),
        (Method::POST, format!("/projects/{CS}/branches/{traversal}/validate")),
        (Method::DELETE, format!("/projects/{CS}/branches/{traversal}")),
        (Method::GET, format!("/projects/{CS}/branches/{traversal}/fields")),
        (Method::GET, format!("/projects/{CS}/branches/{traversal}")),
        (Method::POST, "/projects/..%2Fbilling/branches/main/deploy".to_string()),
        (Method::POST, "/projects/Customer-Service/branches/main/deploy".to_string()),
        (Method::POST, format!("/projects/{CS}/branches/feature%2Fx/deploy")),
        (Method::POST, format!("/projects/{CS}/branches/.hidden/deploy")),
    ] {
        let body = (method == Method::POST).then(|| ("application/gzip", archive(CS, "evil")));
        let a = s.send(method.clone(), &path, &[], body).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{method} {path}: {} {}", a.status, a.error());
        assert_eq!(a.class(), "Invalid", "{method} {path}");
    }
    assert_eq!(std::fs::read(&other).unwrap(), before, "the other deployment is untouched");
    assert_eq!(s.on_disk(), ["default/billing/main", "default/customer-service/main"]);
    assert_eq!(s.keys_in_memory(), ["default/billing/main", "default/customer-service/main"]);
}

/// What is inside an archive cannot reach outside it: no links, no
/// imports beyond the project, no server paths in what comes back, and
/// a validate leaves nothing behind.
#[tokio::test]
async fn an_archive_cannot_reach_outside_itself() {
    let s = server();
    let a = s.deploy("billing", "main", archive("billing", "kept")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());

    // Something on the host that must never be read: here, a file with
    // a marker, standing in for anything else on the machine.
    let outside = s.data_dir.join("outside.yml");
    std::fs::write(
        &outside,
        "fields:\n  - type: dimension\n    name: LEAKED_FROM_HOST\n    data_type: string\n    expression:\n      sql: x\n",
    )
    .unwrap();

    // A symbolic link entry.
    let a = s.validate(CS, "main", archive_with_link(CS, "tests/leak.yml", outside.to_str().unwrap())).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.error());
    assert!(a.error().contains("not a file or a directory"), "{}", a.error());

    // An import by absolute path.
    let mut files = fixture_files(CS, "importer");
    files[2].1 = format!("imports:\n  - {}\n{}", outside.display(), files[2].1);
    let a = s.validate(CS, "main", archive_of(&files)).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.error());
    assert!(a.error().contains("Import outside the project"), "{}", a.error());
    assert!(!a.error().contains("LEAKED_FROM_HOST"), "nothing read is shown: {}", a.error());

    // An import by relative path that climbs out. The work directory
    // and this test's data directory are siblings under the temp
    // directory, so from `models/` two steps up and over reaches it.
    let mut files = fixture_files(CS, "climber");
    files[2].1 = format!("imports:\n  - ../../{}/outside.yml\n{}", s.data_dir.file_name().unwrap().to_string_lossy(), files[2].1);
    let a = s.validate(CS, "main", archive_of(&files)).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.error());
    assert!(a.error().contains("Import outside the project"), "{}", a.error());
    assert!(!a.error().contains("LEAKED_FROM_HOST"), "{}", a.error());

    // A broken project's error names its own files, not the server's.
    let mut files = fixture_files(CS, "broken");
    files[2].1 = "name: Sales\nphysical_name: sales\ndatasource: nowhere\nfields: []\n".into();
    let a = s.validate(CS, "main", archive_of(&files)).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);
    assert!(!a.error().contains("zsqld-deploy"), "the work directory stays private: {}", a.error());

    // A good validate answers and keeps nothing either.
    let a = s.validate(CS, "main", archive(CS, "checked")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert_eq!(a.body["validated_only"], true);

    assert_eq!(s.listing().await, ["billing/main"]);
    assert_eq!(s.on_disk(), ["default/billing/main"]);
    assert_eq!(s.keys_in_memory(), ["default/billing/main"]);
}

/// An archive entry that would land above the project is refused and
/// named, rather than silently left out of the deploy.
#[tokio::test]
async fn an_entry_above_the_project_is_refused() {
    let s = server();
    // The tar crate refuses to write such a path itself, so the name
    // goes into the header's bytes directly, as an attacker's tool would.
    let files = fixture_files(CS, "one");
    let mut buf = Vec::new();
    {
        let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::fast());
        let mut tar = tar::Builder::new(enc);
        for (path, text) in &files {
            let mut h = tar::Header::new_gnu();
            h.set_size(text.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, path, text.as_bytes()).unwrap();
        }
        let text = b"name: escape\n";
        let mut h = tar::Header::new_gnu();
        let name = b"../escape.yml";
        h.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
        h.set_size(text.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append(&h, &text[..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }
    let a = s.validate(CS, "main", buf).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.error());
    assert!(a.error().contains("outside the project") || a.error().contains(".."), "{}", a.error());
}

/// Decompression stops at the limit with a message that says so,
/// whatever the archive claimed.
#[test]
fn decompression_stops_at_the_limit() {
    use std::io::Read;
    let tripped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut capped = Capped { inner: std::io::repeat(0u8), left: UNPACKED_LIMIT, tripped: tripped.clone() };
    let mut sink = [0u8; 1 << 16];
    let mut total = 0u64;
    let err = loop {
        match capped.read(&mut sink) {
            Ok(n) => total += n as u64,
            Err(e) => break e,
        }
    };
    assert_eq!(total, UNPACKED_LIMIT);
    assert!(tripped.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(err.kind(), std::io::ErrorKind::Other);
}

/// Every refusal is `{"error": {"class", "message"}}`, including the
/// ones axum itself would make: a body that is not JSON, a media type
/// that is not JSON, a query string that does not parse. A client
/// reads one shape from every answer.
#[tokio::test]
async fn refusals_from_the_extractors_are_in_the_api_shape() {
    let s = server();
    let a = s.deploy(CS, "main", archive(CS, "one")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());

    let b = format!("/projects/{CS}/branches/main");
    let a = s.send(Method::POST, &format!("{b}/sql"), &[], Some(("application/json", b"not json".to_vec()))).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);
    assert_eq!(a.class(), "Invalid", "{}", a.body);
    assert!(a.headers["content-type"].to_str().unwrap().starts_with("application/json"));

    let a = s.send(Method::POST, &format!("{b}/sql"), &[], Some(("text/plain", b"{}".to_vec()))).await;
    assert_eq!(a.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(a.class(), "Invalid", "{}", a.body);

    let a = s.get(&format!("{b}/fields?hidden=maybe")).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);
    assert_eq!(a.class(), "Invalid", "{}", a.body);
}

/// The context a REST request sends is the context its plan runs
/// under: the policy filters by it, denies without it, and a different
/// caller's context gets a different plan from the same server.
#[tokio::test]
async fn a_rest_plan_runs_under_the_context_the_request_sends() {
    let s = server();
    let a = s.deploy(CS, "main", archive_with_policies(CS, "one")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let sql = format!("/projects/{CS}/branches/main/sql");
    let plan = |context: Value| {
        let (s, sql) = (&s, &sql);
        async move {
            let a = s.post(sql, json!({ "expr": "Revenue", "context": context })).await;
            assert_eq!(a.status, StatusCode::OK, "{}", a.error());
            a.body["sql"].as_str().unwrap().to_lowercase()
        }
    };
    assert!(plan(json!({ "groups": [{ "name": "emea" }] })).await.contains("in ('emea')"));
    assert!(plan(json!({ "groups": [{ "name": "apac" }] })).await.contains("in ('apac')"));
    assert!(plan(json!({})).await.contains("1 = 0"), "a context that resolves nothing is denied");
    // No context at all is not a plan with nobody in it: it is refused.
    let a = s.post(&sql, json!({ "expr": "Revenue" })).await;
    assert_eq!((a.status, a.class()), (StatusCode::BAD_REQUEST, "ContextRequired"), "{}", a.body);
}

/// Over MCP the context comes from the connection's header and from
/// nowhere else: a tool's arguments may not carry one, and a header
/// that is not a context is refused rather than read as none.
#[tokio::test]
async fn an_mcp_plan_runs_under_the_context_the_connection_carries() {
    let s = server();
    let a = s.deploy(CS, "main", archive_with_policies(CS, "one")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let sql_of = |a: &Answer| a.body["result"]["structuredContent"]["sql"].as_str().unwrap_or("").to_lowercase();

    // The header decides.
    let a = s.tool(&[(mcp::CONTEXT_HEADER, EMEA)], "sql", json!({ "project": CS, "spec": spec_of("Revenue") })).await;
    assert!(sql_of(&a).contains("in ('emea')"), "{}", a.body);
    let a = s
        .tool(&[(mcp::CONTEXT_HEADER, r#"{"email":"ann@acme.test"}"#)], "sql", json!({ "project": CS, "spec": spec_of("Category") }))
        .await;
    assert!(sql_of(&a).contains("ann@acme.test"), "{}", a.body);
    let a = s.tool(&[(mcp::CONTEXT_HEADER, EMEA)], "explain", json!({ "project": CS, "spec": spec_of("Revenue") })).await;
    assert!(sql_of(&a).contains("in ('emea')"), "{}", a.body);

    // No header: no context, which a branch with policies refuses, as
    // it does over REST; an empty one resolves nothing and is denied.
    let a = s.tool(&[], "sql", json!({ "project": CS, "spec": spec_of("Revenue") })).await;
    assert_eq!(a.body["result"]["isError"], true, "{}", a.body);
    assert!(a.body["result"]["content"][0]["text"].as_str().unwrap().starts_with("ContextRequired"), "{}", a.body);
    let a = s.tool(&[(mcp::CONTEXT_HEADER, "{}")], "sql", json!({ "project": CS, "spec": spec_of("Revenue") })).await;
    assert!(sql_of(&a).contains("1 = 0"), "{}", a.body);

    // A context in the arguments is refused, with or without the header:
    // the model does not choose who it acts as.
    for headers in [&[][..], &[(mcp::CONTEXT_HEADER, EMEA)][..]] {
        for tool in ["sql", "explain", "explore"] {
            let a = s.tool(headers, tool, json!({ "project": CS, "spec": spec_of("Revenue"), "context": { "system_admin": true } })).await;
            assert_eq!(a.body["error"]["code"], -32602, "{tool}: {}", a.body);
            assert!(a.body["error"]["message"].as_str().unwrap().contains("fixed by the connection"), "{}", a.body);
        }
    }

    // A header that is not a context.
    for bad in ["emea", r#"{"group":"emea"}"#, "[]"] {
        let a = s.tool(&[(mcp::CONTEXT_HEADER, bad)], "sql", json!({ "project": CS, "spec": spec_of("Revenue") })).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{bad}: {}", a.body);
        assert!(a.body["error"]["message"].as_str().unwrap().contains("zsql-context"), "{}", a.body);
    }
}

/// A caller's own filter on the policy's context dimension must narrow
/// the rows the policy allows, never replace the policy's filter.
#[tokio::test]
async fn a_user_filter_on_the_context_dimension_cannot_widen_the_policy() {
    let s = server();
    let a = s.deploy(CS, "main", archive_with_policies(CS, "one")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let mut failed = Vec::new();
    for expr in [
        "Revenue, Region = apac",
        "Category, Revenue, Region = apac",
        "Region, Revenue, Region = apac",
        "Revenue, Region in (apac, amer)",
        "Revenue, Region != emea",
        "Revenue, Region is not null",
        "Region, Revenue, Region not in (emea)",
        "Category, Revenue, Region = apac, Category = books",
        "Revenue, Region = apac, Revenue > 10",
        "Category, Revenue desc, Region = apac",
    ] {
        let a = s.tool(&[(mcp::CONTEXT_HEADER, EMEA)], "sql", json!({ "project": CS, "spec": spec_of(expr) })).await;
        let sql = a.body["result"]["structuredContent"]["sql"].as_str().map(str::to_string).unwrap_or_else(|| format!("ERROR {}", a.body));
        let lower = sql.to_lowercase();
        let secured = lower.contains("in ('emea')") || lower.contains("1 = 0");
        let user_filter_present = lower.contains("apac")
            || lower.contains("amer")
            || lower.contains("!= 'emea'")
            || lower.contains("<> 'emea'")
            || lower.contains("is not null")
            || lower.contains("not in");
        eprintln!("--- {expr}\n{sql}\n");
        if !secured || !user_filter_present {
            failed.push(format!("{expr}: secured={secured} user_filter={user_filter_present}"));
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// A measure's own exclusion or inclusion rule on the context dimension
/// outranks a resolved policy filter: the modeler decided that measure
/// is computed without regard to that dimension, and the decision holds
/// for the policy as it does for a caller's filter. Decided 2026-10-09,
/// on purpose: only the deployer of the model can write such a rule, so
/// a caller cannot reach this. A denial still lands as `1 = 0`.
#[tokio::test]
async fn a_measure_that_ignores_the_context_dimension_is_the_modelers_decision() {
    let s = server();
    let a = s.deploy(CS, "main", archive_with_exclusions(CS, "one")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    for expr in ["Total Revenue, Region = apac", "Global Revenue, Region = apac", "Peak Region Revenue, Region = apac"] {
        let a = s.tool(&[(mcp::CONTEXT_HEADER, EMEA)], "sql", json!({ "project": CS, "spec": spec_of(expr) })).await;
        let sql = a.body["result"]["structuredContent"]["sql"].as_str().unwrap_or("").to_lowercase();
        assert!(sql.contains("sum(t0.\"revenue\")"), "{expr}: {}", a.body);
        assert!(
            !sql.contains("in ('emea')") && !sql.contains("apac"),
            "{expr}: the measure ignores both the caller's filter and the policy's: {sql}"
        );
    }
    // A caller whose context resolves nothing is still denied.
    let a = s.tool(&[(mcp::CONTEXT_HEADER, "{}")], "sql", json!({ "project": CS, "spec": spec_of("Global Revenue") })).await;
    assert!(a.body["result"]["structuredContent"]["sql"].as_str().unwrap_or("").contains("1 = 0"), "{}", a.body);
}

/// The transport: the handshake, the versions, what is not offered,
/// and what a malformed message gets.
#[tokio::test]
async fn the_mcp_wire() {
    let s = server();
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "test", "version": "0" } } });

    // initialize answers the version asked for, and names the server.
    let a = s.mcp(&[], init.clone()).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(a.body["result"]["serverInfo"]["name"], "0sql");
    assert!(a.body["result"]["instructions"].as_str().unwrap().contains("No branch is deployed yet."), "{}", a.body);
    let a = s.mcp(&[], json!({ "jsonrpc": "2.0", "id": 2, "method": "initialize", "params": { "protocolVersion": "1999-01-01" } })).await;
    assert_eq!(a.body["result"]["protocolVersion"], "2025-11-25", "an unknown version gets the newest");

    // Seven tools, none of which takes a context or changes anything.
    let a = s.mcp(&[], json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/list" })).await;
    let tools = a.body["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["list_projects", "fields", "tables", "values", "explore", "sql", "explain"]);
    for t in tools {
        assert!(t["inputSchema"]["properties"].get("context").is_none(), "{t}");
        assert_eq!(t["inputSchema"]["additionalProperties"], false, "{t}");
    }

    // An Authorization header, of any kind, is simply not read here.
    let a = s.mcp(&[("authorization", "Bearer anything")], init.clone()).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);

    // OAuth discovery finds nothing, plainly.
    for path in ["/.well-known/oauth-protected-resource", "/.well-known/oauth-authorization-server"] {
        assert_eq!(s.get(path).await.status, StatusCode::NOT_FOUND);
    }

    // No stream: GET and DELETE are not offered.
    assert_eq!(s.get("/mcp").await.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(s.send(Method::DELETE, "/mcp", &[], None).await.status, StatusCode::METHOD_NOT_ALLOWED);

    // A notification is taken, a batch is not, an unknown method and an
    // unsupported version header are named.
    let a = s.mcp(&[], json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;
    assert_eq!(a.status, StatusCode::ACCEPTED);
    let a = s.mcp(&[], json!([init.clone()])).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);
    let a = s.mcp(&[], json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list" })).await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(a.body["error"]["code"], -32601);
    let a = s.mcp(&[("mcp-protocol-version", "2024-11-05")], init.clone()).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);
    let a = s.mcp(&[("mcp-protocol-version", "2025-11-25")], json!({ "jsonrpc": "2.0", "id": 4, "method": "ping" })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["result"], json!({}));
    let a = s.tool(&[], "deploy", json!({})).await;
    assert_eq!(a.body["error"]["code"], -32602, "there is no tool that writes: {}", a.body);
}

/// What a client is told on connect and by a bare fields call: the
/// catalog of the deployed branches, with uids; the branch a tool works
/// on when none is named; and a refused field name coming back with
/// the fields nearest it.
#[tokio::test]
async fn the_catalog_the_default_branch_and_a_refusal_with_the_nearest_fields() {
    let s = server();
    let a = s.deploy(CS, "main", archive(CS, "one")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s.deploy("billing", "main", archive("billing", "two")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } });

    let a = s.mcp(&[], init).await;
    let text = a.body["result"]["instructions"].as_str().unwrap();
    assert!(text.contains("## Project billing, branch main") && text.contains("## Project customer-service, branch main"), "{text}");
    assert!(text.contains("- revenue: Revenue (decimal)"), "{text}");
    assert!(text.contains("Marker one (marker-one)"), "{text}");
    let a = s.tool(&[], "list_projects", json!({})).await;
    let listed: Vec<String> = a.body["result"]["structuredContent"]["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| format!("{}/{}", d["project"].as_str().unwrap(), d["branch"].as_str().unwrap()))
        .collect();
    assert_eq!(listed, ["billing/main", "customer-service/main"]);

    // fields with no q is the same catalog as text; with q it is the search.
    let a = s.tool(&[], "fields", json!({ "project": CS })).await;
    let r = &a.body["result"];
    assert!(r["isError"].is_null(), "{r}");
    assert!(r["content"][0]["text"].as_str().unwrap().starts_with("## Project customer-service, branch main"), "{r}");
    assert!(r["structuredContent"]["catalog"].is_string());
    let a = s.tool(&[], "fields", json!({ "project": CS, "q": "revenue" })).await;
    assert_eq!(a.body["result"]["structuredContent"]["fields"][0]["uid"], "revenue", "{}", a.body);

    // With no branch named: the one deployed; among several, the
    // project's production branch; and when that is not among them, a
    // request to name one.
    let a = s.deploy(CS, "dev", archive(CS, "dev")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s.tool(&[], "fields", json!({ "project": CS })).await;
    let text = a.body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("## Project customer-service, branch main") && text.contains("Marker one"), "{text}");
    let a = s.tool(&[], "fields", json!({ "project": CS, "branch": "dev" })).await;
    assert!(a.body["result"]["content"][0]["text"].as_str().unwrap().contains("Marker dev"), "{}", a.body);
    let a = s.send(Method::DELETE, &format!("/projects/{CS}/branches/main"), &[], None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s.deploy(CS, "staging", archive(CS, "staging")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s.tool(&[], "tables", json!({ "project": CS })).await;
    assert_eq!(a.body["result"]["isError"], true, "{}", a.body);
    assert!(a.body["result"]["content"][0]["text"].as_str().unwrap().contains("name one"), "{}", a.body);
    let a = s.tool(&[], "tables", json!({ "project": "nothing-here" })).await;
    assert!(a.body["result"]["content"][0]["text"].as_str().unwrap().starts_with("NotFound"), "{}", a.body);

    // A near miss with one clear winner is corrected and said so under
    // corrections; one the planner cannot settle between two candidates
    // is refused, and the refusal carries the nearest fields.
    let a = s
        .tool(
            &[],
            "sql",
            json!({ "project": "billing", "spec": { "projections": [{ "field": "Category" }, { "field": "Revenue Grand Total" }] } }),
        )
        .await;
    assert_eq!(a.body["result"]["structuredContent"]["corrections"][0]["field_uid"], "revenue", "{}", a.body);
    // A model with two revenues and no plain one, so "Revenue Total" has
    // two candidates at the same distance and no containing match.
    let mut files = fixture_files("billing", "two-revenues");
    files.iter_mut().find(|(p, _)| p == "models/tbl.sales.yml").unwrap().1 =
        "name: Sales\nphysical_name: sales\ndatasource: dw\ncost: 10\nfields:\n\
         \x20 - type: dimension\n    name: Category\n    data_type: string\n    expression:\n      sql: category\n\
         \x20 - type: measure\n    name: Revenue Net\n    data_type: decimal\n    expression:\n      sql: sum(net)\n\
         \x20 - type: measure\n    name: Revenue Gross\n    data_type: decimal\n    expression:\n      sql: sum(gross)\n"
            .into();
    files.retain(|(p, _)| p != "tests/smoke.yml");
    let a = s.deploy("billing", "main", archive_of(&files)).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s
        .tool(
            &[],
            "sql",
            json!({ "project": "billing", "spec": { "projections": [{ "field": "Category" }, { "field": "Revenue Total" }] } }),
        )
        .await;
    let r = &a.body["result"];
    assert_eq!(r["isError"], true, "{r}");
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("FieldNotFound"), "{text}");
    assert!(
        text.contains("Fields near 'Revenue Total'")
            && text.contains("- revenue-net: Revenue Net (measure, decimal)")
            && text.contains("- revenue-gross: Revenue Gross (measure, decimal)"),
        "{text}"
    );
}

/// The root and the liveness route answer without a seat, a token or a
/// model.
#[tokio::test]
async fn liveness_needs_nothing() {
    let s = server();
    let res = s.router.clone().oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(&axum::body::to_bytes(res.into_body(), 1024).await.unwrap()[..], b"ok");
    assert_eq!(s.get("/").await.status, StatusCode::OK);
    for gone in ["/app/", "/login", "/signup", "/me", "/account/keys"] {
        assert_eq!(s.get(gone).await.status, StatusCode::NOT_FOUND, "{gone}");
    }
}

// ------------------------------------------------------------------- lists

/// Every list read takes `q` and keeps what holds it, over REST and as
/// the MCP tools; a term too long to be a search is refused.
#[tokio::test]
async fn every_list_takes_a_q() {
    let s = server();
    for branch in ["main", "staging"] {
        let a = s.deploy(CS, branch, archive(CS, "one")).await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    }
    let deployments = |a: &Answer| -> Vec<String> {
        a.body["deployments"].as_array().expect("deployments").iter().map(|d| d["branch"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(deployments(&s.get("/projects").await), ["main", "staging"]);
    assert_eq!(deployments(&s.get("/projects?q=STAG").await), ["staging"]);
    assert_eq!(deployments(&s.get("/projects?q=customer").await), ["main", "staging"], "the project's name counts too");
    assert!(deployments(&s.get("/projects?q=nowhere").await).is_empty());

    let base = format!("/projects/{CS}/branches/main");
    let count = |a: &Answer, list: &str| a.body[list].as_array().map(Vec::len);
    assert_eq!(count(&s.get(&format!("{base}/tables")).await, "tables"), Some(1));
    assert_eq!(count(&s.get(&format!("{base}/tables?q=SAL")).await, "tables"), Some(1));
    assert_eq!(count(&s.get(&format!("{base}/tables?q=nowhere")).await, "tables"), Some(0));
    assert_eq!(count(&s.get(&format!("{base}/datasources")).await, "datasources"), Some(1));
    let a = s.get(&format!("{base}/datasources?q=duck")).await;
    assert_eq!(
        a.body["datasources"][0],
        json!({
            "uid": "dw", "name": "Warehouse", "adapter": "duckdb", "tier": "hot", "description": null,
            "query_timeout": null, "extra_query_params": {}, "connection": {}, "secrets": [],
        })
    );
    assert_eq!(count(&s.get(&format!("{base}/datasources?q=nowhere")).await, "datasources"), Some(0));

    let long = "x".repeat(201);
    for path in [
        format!("/projects?q={long}"),
        format!("{base}/tables?q={long}"),
        format!("{base}/datasources?q={long}"),
        format!("{base}/values/Category?q={long}"),
    ] {
        let a = s.get(&path).await;
        assert_eq!((a.status, a.class()), (StatusCode::BAD_REQUEST, "TooLarge"), "{path}");
    }

    let listed = |a: &Answer, list: &str| a.body["result"]["structuredContent"][list].as_array().map(Vec::len);
    assert_eq!(listed(&s.tool(&[], "list_projects", json!({ "q": "stag" })).await, "deployments"), Some(1));
    assert_eq!(listed(&s.tool(&[], "list_projects", json!({})).await, "deployments"), Some(2));
    assert_eq!(listed(&s.tool(&[], "tables", json!({ "project": CS, "q": "nowhere" })).await, "tables"), Some(0));
    assert_eq!(listed(&s.tool(&[], "tables", json!({ "project": CS, "q": "sales" })).await, "tables"), Some(1));
}

// ------------------------------------------------------------------ probes

/// A warehouse in a file: two schemas, a name in both, comments and keys.
fn warehouse(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zsqld-probe-{name}-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dw.duckdb");
    let db = ::duckdb::Connection::open(&path).unwrap();
    db.execute_batch(
        "CREATE SCHEMA dse;\
         CREATE TABLE dse.cs_agent_d (id INTEGER PRIMARY KEY, name VARCHAR(80) NOT NULL);\
         CREATE TABLE dse.cs_contact_f (id INTEGER PRIMARY KEY, agent_id INTEGER NOT NULL REFERENCES dse.cs_agent_d (id), \
           opened DATE NOT NULL, note VARCHAR(40));\
         COMMENT ON TABLE dse.cs_contact_f IS 'One row per contact';\
         COMMENT ON COLUMN dse.cs_contact_f.note IS 'What the agent wrote';\
         CREATE VIEW dse.cs_recent AS SELECT id FROM dse.cs_contact_f;\
         CREATE TABLE main.cs_contact_f (id INTEGER);\
         CREATE TABLE main.orders (id INTEGER);",
    )
    .unwrap();
    path
}

/// A datasource is probed with the connection the request carries: a
/// page of its tables, narrowed by schema and name, and one table's
/// structure. Nothing about the connection is deployed, and none of it
/// comes back.
#[tokio::test]
async fn a_probe_reads_the_warehouse_the_request_points_at() {
    let s = server();
    let a = s
        .send(
            Method::POST,
            &format!("/projects/{CS}/branches/main/deploy"),
            &[("authorization", "Bearer s3cret")],
            Some(("application/gzip", archive(CS, "one"))),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let file = warehouse("reads");
    let conn = json!({ "connection": { "file": file.to_str().unwrap(), "password": "not-needed-and-never-echoed" } });
    let base = format!("/projects/{CS}/branches/main/datasources/dw");
    let names = |a: &Answer| -> Vec<String> {
        a.body["tables"].as_array().expect("tables").iter().map(|t| t["physical_name"].as_str().unwrap().to_string()).collect()
    };

    // The whole listing, by schema then name, tables only.
    let a = s.post(&format!("{base}/tables"), conn.clone()).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert_eq!(names(&a), ["dse.cs_agent_d", "dse.cs_contact_f", "dse.cs_recent", "main.cs_contact_f", "main.orders"]);
    assert_eq!((&a.body["limit"], &a.body["offset"], &a.body["next_offset"]), (&json!(100), &json!(0), &Value::Null));
    assert_eq!((&a.body["datasource"], &a.body["adapter"]), (&json!("dw"), &json!("duckdb")));
    let contact = &a.body["tables"][1];
    assert_eq!(
        (&contact["kind"], &contact["comment"], &contact["schema"]),
        (&json!("table"), &json!("One row per contact"), &json!("dse"))
    );
    assert!(contact.get("columns").is_none(), "a listing reads nothing below a table");
    assert_eq!(a.body["tables"][2]["kind"], "view");
    assert!(!a.body.to_string().contains("never-echoed"), "nothing of the connection comes back");

    // Pages: next_offset is where the next one starts, null on the last.
    let mut seen = Vec::new();
    let mut offset = json!(0);
    while !offset.is_null() {
        let a = s.post(&format!("{base}/tables?limit=2&offset={offset}"), conn.clone()).await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.error());
        seen.extend(names(&a));
        offset = a.body["next_offset"].clone();
    }
    assert_eq!(seen.len(), 5, "{seen:?}");

    // Narrowed by schema, by name, by both; paged while narrowed.
    assert_eq!(names(&s.post(&format!("{base}/tables?schema=main"), conn.clone()).await), ["main.cs_contact_f", "main.orders"]);
    assert_eq!(names(&s.post(&format!("{base}/tables?q=CONTACT"), conn.clone()).await), ["dse.cs_contact_f", "main.cs_contact_f"]);
    assert_eq!(names(&s.post(&format!("{base}/tables?q=cs_&schema=dse&limit=1&offset=1"), conn.clone()).await), ["dse.cs_contact_f"]);
    assert!(names(&s.post(&format!("{base}/tables?q=nowhere"), conn.clone()).await).is_empty());

    // One table, named by the path or with its schema beside it.
    for path in [
        format!("{base}/tables/dse.cs_contact_f"),
        format!("{base}/tables/cs_contact_f?schema=dse"),
        format!("{base}/tables/CS_CONTACT_F?schema=dse"),
    ] {
        let a = s.post(&path, conn.clone()).await;
        assert_eq!(a.status, StatusCode::OK, "{path}: {}", a.error());
        let t = &a.body["table"];
        assert_eq!((&t["physical_name"], &t["comment"]), (&json!("dse.cs_contact_f"), &json!("One row per contact")), "{path}");
        let columns: Vec<(&str, &str, bool)> = t["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["name"].as_str().unwrap(), c["data_type"].as_str().unwrap(), c["nullable"].as_bool().unwrap()))
            .collect();
        assert_eq!(
            columns,
            [("id", "INTEGER", false), ("agent_id", "INTEGER", false), ("opened", "DATE", false), ("note", "VARCHAR", true)]
        );
        assert_eq!(t["columns"][3]["comment"], "What the agent wrote");
        assert_eq!(t["primary_key"], json!(["id"]));
        assert_eq!(t["foreign_keys"][0]["references_table"], "cs_agent_d");
        assert!(a.body["read_at"].as_str().is_some_and(|at| at.ends_with('Z')));
    }

    // A name in two schemas is not answered with the first.
    let a = s.post(&format!("{base}/tables/cs_contact_f"), conn.clone()).await;
    assert_eq!((a.status, a.class()), (StatusCode::BAD_REQUEST, "Ambiguous"));
    assert!(a.error().contains("dse.cs_contact_f") && a.error().contains("main.cs_contact_f"), "{}", a.error());
    let a = s.post(&format!("{base}/tables/dse.nothing"), conn.clone()).await;
    assert_eq!((a.status, a.class()), (StatusCode::NOT_FOUND, "NotFound"), "{}", a.error());
    assert!(a.error().contains("in schema dse"), "{}", a.error());

    // An engine without catalogs says so, in the query or in the path.
    for path in [format!("{base}/tables?catalog=prod"), format!("{base}/tables/prod.dse.cs_contact_f")] {
        let a = s.post(&path, conn.clone()).await;
        assert_eq!((a.status, a.class()), (StatusCode::BAD_REQUEST, "DatasourceError"), "{path}");
        assert!(a.error().contains("has no catalogs"), "{}", a.error());
    }

    // What is wrong with a request is said before anything is opened.
    let refused = |a: Answer, status: StatusCode, class: &str, says: &str| {
        assert_eq!((a.status, a.class()), (status, class), "{}", a.error());
        assert!(a.error().contains(says), "wanted {says:?} in {:?}", a.error());
    };
    refused(s.post(&format!("{base}/tables?limit=101"), conn.clone()).await, StatusCode::BAD_REQUEST, "TooLarge", "the limit is 100");
    refused(s.post(&format!("{base}/tables?limit=0"), conn.clone()).await, StatusCode::BAD_REQUEST, "Invalid", "at least 1");
    refused(s.post(&format!("{base}/tables?schemas=main"), conn.clone()).await, StatusCode::BAD_REQUEST, "Invalid", "schemas");
    refused(
        s.post(&format!("{base}/tables"), json!({ "connection": { "flie": "x" } })).await,
        StatusCode::BAD_REQUEST,
        "Invalid",
        "connection.flie is not a connection setting",
    );
    refused(
        s.post(&format!("{base}/tables"), json!({ "connection": { "file": "x", "adapter": "postgres" } })).await,
        StatusCode::BAD_REQUEST,
        "Invalid",
        "deployed as duckdb",
    );
    refused(
        s.post(&format!("{base}/tables"), json!({ "connection": { "port": "five" } })).await,
        StatusCode::BAD_REQUEST,
        "Invalid",
        "connection.port must be a number",
    );
    refused(
        s.post(&format!("/projects/{CS}/branches/main/datasources/lake/tables"), conn.clone()).await,
        StatusCode::NOT_FOUND,
        "NotFound",
        "no datasource lake (it has: dw)",
    );
    refused(
        s.post(&format!("/projects/{CS}/branches/nowhere/datasources/dw/tables"), conn.clone()).await,
        StatusCode::NOT_FOUND,
        "NotFound",
        "no deployment",
    );
    // A warehouse that is not there is named in the refusal.
    let absent = file.with_file_name("absent.duckdb");
    let a = s.post(&format!("{base}/tables"), json!({ "connection": { "file": absent.to_str().unwrap() } })).await;
    assert_eq!(a.class(), "DatasourceError", "{:?}", a.body);
    assert!(a.error().contains("absent.duckdb"), "{}", a.error());
    // No body is what was deployed, and this deploy said nothing of where.
    let a = s.send(Method::POST, &format!("{base}/tables"), &[], None).await;
    assert_eq!((a.status, a.class()), (StatusCode::BAD_REQUEST, "DatasourceError"), "{:?}", a.body);

    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

/// What a datasource's entry says when it is deployed is kept, every key
/// of it, and is the connection a probe uses: nothing need be sent. The
/// request says only what differs. A secret deployed in the entry opens
/// the warehouse and is never served.
#[tokio::test]
async fn a_datasource_is_kept_as_it_was_deployed_and_probed_with_it() {
    let s = server();
    let file = warehouse("deployed");
    let mut files = fixture_files(CS, "one");
    files.iter_mut().find(|(p, _)| p == "datasources.yml").unwrap().1 = format!(
        "dw:\n  name: Warehouse\n  adapter: duckdb\n  tier: hot\n  query_timeout: 30\n  file: {}\n  \
         password: hunter2-never-served\n  team: data\n  oauth:\n    client_secret: nested-never-served\n    scope: read\n",
        file.display()
    );
    let a = s.deploy(CS, "main", archive_of(&files)).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let base = format!("/projects/{CS}/branches/main/datasources/dw");
    let names = |a: &Answer| -> Vec<String> {
        a.body["tables"].as_array().expect("tables").iter().map(|t| t["physical_name"].as_str().unwrap().to_string()).collect()
    };

    // Every key the entry carried is there to read, but for the secrets,
    // which are named and not shown.
    let a = s.get(&format!("/projects/{CS}/branches/main/datasources")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let dw = &a.body["datasources"][0];
    assert_eq!((&dw["adapter"], &dw["query_timeout"]), (&json!("duckdb"), &json!(30)));
    assert_eq!(dw["connection"], json!({ "file": file.to_str().unwrap(), "team": "data", "oauth": { "scope": "read" } }));
    assert_eq!(dw["secrets"], json!(["oauth.client_secret", "password"]));
    assert!(!a.body.to_string().contains("never-served"), "{}", a.body);

    // No body, an empty one, and an empty connection all probe what was deployed.
    let a = s.send(Method::POST, &format!("{base}/tables?schema=main"), &[], None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert_eq!(names(&a), ["main.cs_contact_f", "main.orders"]);
    assert!(!a.body.to_string().contains("never-served"));
    for body in [json!({}), json!({ "connection": {} })] {
        let a = s.post(&format!("{base}/tables?schema=main"), body).await;
        assert_eq!(names(&a), ["main.cs_contact_f", "main.orders"]);
    }
    let a = s.send(Method::POST, &format!("{base}/tables/dse.cs_contact_f"), &[], None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert_eq!(a.body["table"]["primary_key"], json!(["id"]));

    // What the request sends wins over what was deployed, key by key.
    let absent = file.with_file_name("absent.duckdb");
    let a = s.post(&format!("{base}/tables"), json!({ "connection": { "file": absent.to_str().unwrap() } })).await;
    assert_eq!(a.class(), "DatasourceError", "{:?}", a.body);
    assert!(a.error().contains("absent.duckdb"), "{}", a.error());
    // A key the deploy could carry is still not one a request may invent.
    let a = s.post(&format!("{base}/tables"), json!({ "connection": { "team": "ops" } })).await;
    assert_eq!((a.status, a.class()), (StatusCode::BAD_REQUEST, "Invalid"));
    // And a body that is not JSON is refused without being quoted back.
    let a = s.send(Method::POST, &format!("{base}/tables"), &[], Some(("application/json", b"{\"connection\": hunter3".to_vec()))).await;
    assert_eq!((a.status, a.class()), (StatusCode::BAD_REQUEST, "Invalid"));
    assert!(!a.error().contains("hunter3"), "{}", a.error());

    // It is in the snapshot, so a restart has it too.
    let again = app_over(&s.data_dir);
    reload(&again).unwrap();
    let again = Server { router: router(again.clone()), app: again, data_dir: s.data_dir.clone() };
    let a = again.send(Method::POST, &format!("{base}/tables?schema=main"), &[], None).await;
    assert_eq!(names(&a), ["main.cs_contact_f", "main.orders"]);

    // A deploy that says nothing of where the warehouse is leaves it to the request.
    let a = s.deploy(CS, "bare", archive(CS, "one")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s.send(Method::POST, &format!("/projects/{CS}/branches/bare/datasources/dw/tables"), &[], None).await;
    assert_eq!((a.status, a.class()), (StatusCode::BAD_REQUEST, "DatasourceError"), "{:?}", a.body);
    assert!(a.error().contains("deploy it in datasources.yml or send it"), "{}", a.error());
}

// -------------------------------------------------------------------- runs

/// The fixture's warehouse in a file: the `sales` table the model reads.
fn sales(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zsqld-run-{name}-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dw.duckdb");
    let db = ::duckdb::Connection::open(&path).unwrap();
    db.execute_batch(
        "CREATE TABLE sales (category VARCHAR, marker_one VARCHAR, revenue DECIMAL(12, 2));\
         INSERT INTO sales VALUES ('books', 'm', 10.50), ('books', 'm', 20.00), ('games', 'm', 5.25), ('toys', NULL, 1.00);",
    )
    .unwrap();
    path
}

/// A plan is run: the rows come back with their types, the record says
/// where they came from, an identical question is answered from what is
/// stored, and a result is read again by its id, a page at a time or
/// whole as Arrow.
#[tokio::test]
async fn a_run_answers_with_rows_and_is_found_again() {
    let s = server();
    assert_eq!(s.deploy(CS, "main", archive(CS, "one")).await.status, StatusCode::OK);
    let file = sales("rows");
    let run = format!("/projects/{CS}/branches/main/run");
    let ask = |extra: Value| {
        let mut body =
            json!({ "expr": "category asc, revenue", "connection": { "file": file.to_str().unwrap(), "password": "never-echoed" } });
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        body
    };

    let a = s.post(&run, ask(json!({}))).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!((&a.body["status"], &a.body["cache"], &a.body["type"]), (&json!("succeeded"), &json!("miss"), &json!("semantic")));
    assert_eq!((&a.body["row_count"], &a.body["complete"]), (&json!(3), &json!(true)));
    assert_eq!(a.body["columns"][0]["type"], "string");
    assert_eq!((&a.body["columns"][1]["type"], &a.body["columns"][1]["arrow_type"]), (&json!("decimal"), &json!("Decimal128(38, 2)")));
    assert_eq!(a.body["rows"], json!([["books", "30.50"], ["games", "5.25"], ["toys", "1.00"]]), "a decimal is a string, at its scale");
    assert_eq!(
        (&a.body["project_uid"], &a.body["branch"], &a.body["datasource_uid"], &a.body["org_uid"]),
        (&json!(CS), &json!("main"), &json!("dw"), &json!("default"))
    );
    assert_eq!((&a.body["tables"], &a.body["dirty"], &a.body["row_limit"]), (&json!(["sales"]), &json!(false), &Value::Null));
    assert!(a.body["sql"].as_str().unwrap().contains("sales T0"), "{}", a.body["sql"]);
    assert!(a.body["expires_at"].as_str() > a.body["finished_at"].as_str());
    assert!(!String::from_utf8_lossy(&a.raw).contains("never-echoed"), "a credential is in no answer");
    let id = a.body["id"].as_str().unwrap().to_string();

    assert_eq!(s.app.runs.idle_connections(), 1, "the connection it opened is kept for the next run");
    // The same question again is the stored result; fresh runs anyway.
    let again = s.post(&run, ask(json!({}))).await;
    assert_eq!((&again.body["cache"], again.body["id"].as_str()), (&json!("hit"), Some(id.as_str())));
    assert_eq!(again.body["rows"], a.body["rows"]);
    let fresh = s.post(&run, ask(json!({ "fresh": true }))).await;
    assert_eq!(fresh.body["cache"], "miss");
    assert_ne!(fresh.body["id"], a.body["id"]);
    assert_eq!(s.app.runs.idle_connections(), 1, "and was used again: the same datasource, settings and credential");
    // Another credential is another connection.
    let other = s.post(&run, json!({ "expr": "category asc, revenue", "fresh": true, "connection": { "file": file.to_str().unwrap(), "password": "another" } })).await;
    assert_eq!(other.body["status"], "succeeded", "{}", other.body);
    assert_eq!(s.app.runs.idle_connections(), 2);

    // By its id: a page from the middle, and the whole of it as Arrow.
    let page = s.get(&format!("/results/{id}?offset=1&limit=1")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!((&page.body["rows"], &page.body["complete"], &page.body["offset"]), (&json!([["games", "5.25"]]), &json!(false), &json!(1)));
    assert!(page.body.get("cache").is_none(), "only a request to run says how it was met");
    let last = s.get(&format!("/results/{id}?offset=2")).await;
    assert_eq!((&last.body["rows"], &last.body["complete"]), (&json!([["toys", "1.00"]]), &json!(true)));
    let arrow = s.send(Method::GET, &format!("/results/{id}"), &[("accept", "application/vnd.apache.arrow.stream")], None).await;
    assert_eq!(arrow.status, StatusCode::OK);
    assert_eq!(arrow.headers["content-type"], "application/vnd.apache.arrow.stream");
    let read = arrow_ipc::reader::StreamReader::try_new(std::io::Cursor::new(arrow.raw), None).unwrap();
    assert_eq!(read.schema().field(1).data_type().to_string(), "Decimal128(38, 2)");
    assert_eq!(read.map(|b| b.unwrap().num_rows()).sum::<usize>(), 3);

    // A table the result read has changed: it stops answering.
    let invalidate = |body: Value| s.post("/cache/invalidate", body);
    assert_eq!(invalidate(json!({ "datasource": "dw", "tables": ["elsewhere"] })).await.body, json!({ "invalidated": 0 }));
    assert_eq!(invalidate(json!({ "datasource": "dw", "tables": ["sales"], "project": "another" })).await.body["invalidated"], 0);
    let marked = invalidate(json!({ "datasource": "dw", "tables": ["sales"], "project": CS })).await;
    assert_eq!(
        (marked.status, &marked.body["invalidated"]),
        (StatusCode::OK, &json!(3)),
        "the result and the two fresh ones after it: {}",
        marked.body
    );
    assert_eq!(s.get(&format!("/results/{id}")).await.body["dirty"], true);
    let after = s.post(&run, ask(json!({}))).await;
    assert_eq!(after.body["cache"], "miss", "a dirty result does not answer");
    assert_eq!(invalidate(json!({ "datasource": "dw", "tables": [] })).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(invalidate(json!({ "datasource": "", "tables": ["sales"] })).await.status, StatusCode::BAD_REQUEST);
    assert!(invalidate(json!({ "tables": ["sales"] })).await.status.is_client_error());

    // What a request may not ask for, and an id nobody has.
    assert_eq!(s.get(&format!("/results/{id}?limit=0")).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(s.get(&format!("/results/{id}?limit=100001")).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(s.post(&run, ask(json!({ "wait": -1 }))).await.status, StatusCode::BAD_REQUEST);
    // A wait no clock could hold is the longest wait there is, not a crash.
    assert_eq!(s.get(&format!("/results/{id}?wait=1e30")).await.status, StatusCode::OK);
    assert_eq!(s.post(&run, ask(json!({ "wait": 1e30 }))).await.status, StatusCode::OK);
    let misspelled = s.post(&run, ask(json!({ "wiat": 1 }))).await;
    assert!(
        misspelled.status.is_client_error() && misspelled.error().contains("wiat"),
        "a misspelled key is named, not read past: {}",
        misspelled.body
    );
    let none = s.get("/results/r-none").await;
    assert_eq!((none.status, none.class()), (StatusCode::NOT_FOUND, "NotFound"));
    assert_eq!(s.send(Method::DELETE, "/results/r-none", &[], None).await.status, StatusCode::NOT_FOUND);
    // A request that plans to nothing is refused as `sql` refuses it.
    let bad = s.post(&run, json!({ "expr": "no_such_field", "connection": { "file": file.to_str().unwrap() } })).await;
    assert_eq!(bad.status, s.post(&format!("/projects/{CS}/branches/main/sql"), json!({ "expr": "no_such_field" })).await.status);
}

/// A request that will not wait is answered with the id at once, and
/// the id is asked about until the rows are there. A run that fails is
/// a record that says so, not a failed request.
#[tokio::test]
async fn a_run_is_answered_by_its_id_and_a_failure_is_recorded() {
    let s = server();
    assert_eq!(s.deploy(CS, "main", archive(CS, "one")).await.status, StatusCode::OK);
    let file = sales("async");
    let run = format!("/projects/{CS}/branches/main/run");
    let a = s.post(&run, json!({ "expr": "category, revenue", "wait": 0, "connection": { "file": file.to_str().unwrap() } })).await;
    assert_eq!(a.status, StatusCode::ACCEPTED, "{}", a.body);
    assert!(matches!(a.body["status"].as_str(), Some("queued" | "running")), "{}", a.body);
    assert!(a.body.get("rows").is_none());
    let id = a.body["id"].as_str().unwrap().to_string();
    let done = s.get(&format!("/results/{id}?wait=20")).await;
    assert_eq!((done.status, &done.body["status"], &done.body["row_count"]), (StatusCode::OK, &json!("succeeded"), &json!(3)));
    // Over already: cancelling changes nothing and says how it ended.
    let late = s.send(Method::DELETE, &format!("/results/{id}"), &[], None).await;
    assert_eq!((late.status, &late.body["status"]), (StatusCode::OK, &json!("succeeded")));

    // The warehouse is not there. The request is answered; the run failed.
    // (Asked fresh: the cache is found by the project and the statement
    // alone, so without it the result above would answer this too.)
    let absent = file.with_file_name("absent.duckdb");
    let failed =
        s.post(&run, json!({ "expr": "category, revenue", "fresh": true, "connection": { "file": absent.to_str().unwrap() } })).await;
    assert_eq!((failed.status, &failed.body["status"]), (StatusCode::OK, &json!("failed")), "{}", failed.body);
    assert!(failed.body["error"].as_str().unwrap().contains("absent.duckdb"), "{}", failed.body);
    assert!(failed.body.get("rows").is_none() && failed.body["expires_at"].is_null());
    // A failure does not answer the next request.
    let next = s.post(&run, json!({ "expr": "category, revenue", "connection": { "file": file.to_str().unwrap() } })).await;
    assert_eq!(
        (&next.body["status"], &next.body["cache"]),
        (&json!("succeeded"), &json!("hit")),
        "the earlier result still does: {}",
        next.body
    );
}

/// What the server was told about limits: a row limit for a request
/// with none, and a datasource that keeps nothing.
#[tokio::test]
async fn a_default_limit_and_a_datasource_that_keeps_nothing() {
    let data_dir = std::env::temp_dir().join(format!("zsqld-tests-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&data_dir).unwrap();
    let dialects: &'static Dialects = Box::leak(Box::new(Dialects::builtin()));
    let app: Shared = Arc::new(App {
        dialects,
        data_dir: data_dir.clone(),
        models: RwLock::new(HashMap::new()),
        admission: Admission::new(&AdmissionCounts::default()),
        mcp: mcp::Settings::builtin(),
        deploying: std::sync::Mutex::new(()),
        runs: crate::open_runs(&data_dir, &crate::RunLimits::default()).unwrap(),
        default_sql_limit: Some(2),
    });
    let s = Server { router: router(app.clone()), app, data_dir };
    let file = sales("limits");
    // Deployed whole, and saying it keeps nothing.
    let mut files = fixture_files(CS, "one");
    files[1].1 = format!(
        "dw:\n  name: Warehouse\n  adapter: duckdb\n  tier: hot\n  cache_ttl: 0\n  query_wait_time: 30\n  file: {}\n",
        file.display()
    );
    assert_eq!(s.deploy(CS, "main", archive_of(&files)).await.status, StatusCode::OK);
    let run = format!("/projects/{CS}/branches/main/run");

    let a = s.post(&run, json!({ "expr": "category asc, revenue" })).await;
    assert_eq!((a.status, &a.body["status"]), (StatusCode::OK, &json!("succeeded")), "{}", a.body);
    assert_eq!((&a.body["row_limit"], &a.body["row_count"]), (&json!(2), &json!(2)), "the server's limit, for a request with none");
    assert!(a.body["sql"].as_str().unwrap().to_uppercase().contains("LIMIT 2"), "{}", a.body["sql"]);
    assert!(a.body["cache_key"].is_null(), "cache_ttl: 0 keeps nothing");
    let b = s.post(&run, json!({ "expr": "category asc, revenue" })).await;
    assert_eq!(b.body["cache"], "miss", "and so nothing is found again");
    assert_ne!(a.body["id"], b.body["id"]);

    // The request's own limit wins.
    let own = s.post(&run, json!({ "expr": "category asc, revenue, limit 1" })).await;
    assert_eq!((&own.body["row_limit"], &own.body["row_count"]), (&json!(1), &json!(1)), "{}", own.body);
}

/// The caller's own statement, run as it was written: it may write, it
/// is never looked up or found again, and it is one statement.
#[tokio::test]
async fn sql_exec_runs_one_statement_of_the_callers_and_keeps_nothing_of_it_for_reuse() {
    let s = server();
    assert_eq!(s.deploy(CS, "main", archive(CS, "one")).await.status, StatusCode::OK);
    let file = sales("sql-exec");
    let conn = json!({ "file": file.to_str().unwrap() });
    let exec = format!("/projects/{CS}/branches/main/sql-exec");
    let run = format!("/projects/{CS}/branches/main/run");
    let ask = |sql: &str| s.post(&exec, json!({ "datasource": "dw", "sql": sql, "connection": conn }));

    // A plan runs first and leaves a connection kept for reading.
    let before = s.post(&run, json!({ "expr": "category asc, revenue", "connection": conn })).await;
    assert_eq!(before.body["row_count"], 3, "{}", before.body);
    assert_eq!(s.app.runs.idle_connections(), 1);

    // A read: typed as the engine typed it, and never an answer for later.
    let count = ask("SELECT count(*) AS n, sum(revenue) AS total FROM sales").await;
    assert_eq!(
        (count.status, &count.body["status"], &count.body["type"]),
        (StatusCode::OK, &json!("succeeded"), &json!("sql")),
        "{}",
        count.body
    );
    assert_eq!(count.body["rows"], json!([[4, "36.75"]]), "an integer is a number");
    assert_eq!(
        (&count.body["columns"][0]["type"], &count.body["cache_key"], &count.body["tables"]),
        (&json!("integer"), &Value::Null, &json!([]))
    );
    assert!(count.body.get("cache").is_none() && count.body["spec"].is_null());
    let again = ask("SELECT count(*) AS n, sum(revenue) AS total FROM sales").await;
    assert_ne!(again.body["id"], count.body["id"], "two of the same are two runs");

    // A write, on a connection that may: the database decides, not this.
    let wrote = ask("INSERT INTO sales VALUES ('maps', 'm', 2.00), ('maps', 'm', 3.00)").await;
    assert_eq!((wrote.status, &wrote.body["status"]), (StatusCode::OK, &json!("succeeded")), "{}", wrote.body);
    assert_eq!(wrote.body["complete"], true, "a statement that wrote has nothing more to page through: {}", wrote.body);
    assert_eq!(ask("SELECT count(*) AS n FROM sales").await.body["rows"], json!([[6]]));
    assert_eq!(s.app.runs.idle_connections(), 0, "a writable connection to a file is not kept, nor the readers it displaced");
    // The plan, asked fresh, sees what was written.
    let after = s.post(&run, json!({ "expr": "category asc, revenue", "fresh": true, "connection": conn })).await;
    assert_eq!(after.body["row_count"], 4, "{}", after.body);
    assert_eq!(after.body["rows"][2], json!(["maps", "5.00"]));
    // By its id, like any run.
    let id = wrote.body["id"].as_str().unwrap();
    assert_eq!(s.get(&format!("/results/{id}")).await.body["status"], "succeeded");

    // A statement the engine refuses is a run that failed.
    let bad = ask("SELECT nope FROM sales").await;
    assert_eq!((bad.status, &bad.body["status"]), (StatusCode::OK, &json!("failed")));
    assert!(bad.body["error"].as_str().unwrap().contains("nope"), "{}", bad.body);
    // What is not one statement, or names no datasource of the branch.
    let two = ask("SELECT 1; DELETE FROM sales").await;
    assert_eq!(two.status, StatusCode::BAD_REQUEST);
    assert!(two.error().contains("one statement"), "{}", two.error());
    assert_eq!(ask("   ").await.status, StatusCode::BAD_REQUEST);
    assert_eq!(ask("SELECT count(*) AS n FROM sales").await.body["rows"], json!([[6]]), "and none of the two ran");
    let nowhere = s.post(&exec, json!({ "datasource": "elsewhere", "sql": "SELECT 1" })).await;
    assert_eq!(nowhere.status, StatusCode::NOT_FOUND, "{}", nowhere.body);
    assert!(s.post(&exec, json!({ "sql": "SELECT 1" })).await.status.is_client_error());
}

/// The two tools that run a statement are not offered unless they are
/// named: a model that can run what it plans reads rows with the
/// credential the datasource was deployed with. Named, they run a spec and read a result.
#[tokio::test]
async fn the_tools_that_run_are_offered_only_when_named() {
    let listed = |a: &Answer| -> Vec<String> {
        a.body["result"]["tools"].as_array().expect("tools").iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
    };
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let s = server();
    let offered = listed(&s.mcp(&[], list.clone()).await);
    assert!(
        offered.contains(&"sql".to_string()) && !offered.contains(&"run".to_string()) && !offered.contains(&"result".to_string()),
        "{offered:?}"
    );
    let refused = s.tool(&[], "run", json!({ "project": CS, "spec": { "projections": [{ "field": "category" }] } })).await;
    assert!(refused.body["error"]["message"].as_str().unwrap_or_default().contains("no tool run"), "{}", refused.body);

    // A server told to offer them, with the warehouse deployed whole:
    // a tool has no connection to send.
    let data_dir = std::env::temp_dir().join(format!("zsqld-tests-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&data_dir).unwrap();
    let dialects: &'static Dialects = Box::leak(Box::new(Dialects::builtin()));
    let tools = ["list_projects", "fields", "sql", "run", "result"].map(str::to_string).to_vec();
    let app: Shared = Arc::new(App {
        dialects,
        data_dir: data_dir.clone(),
        models: RwLock::new(HashMap::new()),
        admission: Admission::new(&AdmissionCounts::default()),
        mcp: mcp::Settings::new(None, None, Some(tools)).unwrap(),
        deploying: std::sync::Mutex::new(()),
        runs: crate::open_runs(&data_dir, &crate::RunLimits::default()).unwrap(),
        default_sql_limit: None,
    });
    let s = Server { router: router(app.clone()), app, data_dir };
    let file = sales("tools");
    let mut files = fixture_files(CS, "one");
    files[1].1 = format!("dw:\n  name: Warehouse\n  adapter: duckdb\n  tier: hot\n  file: {}\n", file.display());
    assert_eq!(s.deploy(CS, "main", archive_of(&files)).await.status, StatusCode::OK);
    let offered = listed(&s.mcp(&[], list).await);
    assert!(offered.contains(&"run".to_string()) && offered.contains(&"result".to_string()), "{offered:?}");

    let spec = json!({ "projections": [{ "field": "category" }, { "field": "revenue" }] });
    let ran = s.tool(&[], "run", json!({ "project": CS, "spec": spec, "limit": 2 })).await;
    let answer = &ran.body["result"]["structuredContent"];
    assert_eq!(
        (&answer["status"], &answer["row_count"], &answer["complete"], &answer["cache"]),
        (&json!("succeeded"), &json!(3), &json!(false), &json!("miss")),
        "{}",
        ran.body
    );
    assert_eq!(answer["rows"].as_array().unwrap().len(), 2, "the rows this answer carries, not the query's limit");
    let id = answer["id"].as_str().unwrap().to_string();
    let rest = s.tool(&[], "result", json!({ "id": id, "offset": 2 })).await;
    let rest = &rest.body["result"]["structuredContent"];
    assert_eq!((rest["rows"].as_array().unwrap().len(), &rest["complete"]), (1, &json!(true)));
    let again = s.tool(&[], "run", json!({ "project": CS, "spec": spec })).await;
    assert_eq!(again.body["result"]["structuredContent"]["cache"], "hit");
    // A tool reads every row it is handed, so it may ask for fewer.
    let many = s.tool(&[], "run", json!({ "project": CS, "spec": spec, "limit": 5000 })).await;
    assert_eq!(many.body["result"]["isError"], true, "{}", many.body);
    let none = s.tool(&[], "result", json!({ "id": "r-none" })).await;
    assert_eq!(none.body["result"]["isError"], true);
    // A model does not choose whose rows it reads.
    let chosen = s.tool(&[], "run", json!({ "project": CS, "spec": spec, "context": { "email": "a@b.c" } })).await;
    assert!(chosen.body["error"].is_object() || chosen.body["result"]["isError"] == true, "{}", chosen.body);
}

// -------------------------------------------------------------------- orgs

/// The same project and branch under two orgs and under none are three
/// deployments, each in its org's directory: one answers for itself, a
/// listing is one org's, and removing one leaves the others. With no
/// org in the path a request is for `default`, which is also an org by
/// name.
#[tokio::test]
async fn an_org_is_its_own_set_of_projects() {
    let s = server();
    assert_eq!(s.deploy(CS, "main", archive(CS, "plain")).await.status, StatusCode::OK);
    for org in ["acme", "globex"] {
        let a = s.deploy_to(org, CS, "main", archive(CS, org)).await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    }
    let a = s.deploy_to("acme", "billing", "main", archive("billing", "acme-billing")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let all = ["acme/billing/main", "acme/customer-service/main", "default/customer-service/main", "globex/customer-service/main"];
    assert_eq!(s.on_disk(), all);
    assert_eq!(s.keys_in_memory(), all);

    let marker = |a: Answer| -> Vec<String> {
        assert_eq!(a.status, StatusCode::OK, "{}", a.error());
        let names = a.body["fields"].as_array().unwrap().iter().filter_map(|f| f["name"].as_str());
        names.filter(|n| n.starts_with("Marker")).map(str::to_string).collect()
    };
    let fields = format!("projects/{CS}/branches/main/fields?hidden=true");
    assert_eq!(marker(s.get(&format!("/{fields}")).await), ["Marker plain"]);
    assert_eq!(marker(s.get(&format!("/orgs/default/{fields}")).await), ["Marker plain"], "default is the org of a path with none");
    assert_eq!(marker(s.get(&format!("/orgs/acme/{fields}")).await), ["Marker acme"]);
    assert_eq!(marker(s.get(&format!("/orgs/globex/{fields}")).await), ["Marker globex"]);

    let listed = |a: Answer| -> Vec<String> {
        let rows = a.body["deployments"].as_array().unwrap().iter();
        rows.map(|d| format!("{}/{}", d["project"].as_str().unwrap(), d["branch"].as_str().unwrap())).collect()
    };
    assert_eq!(s.listing().await, ["customer-service/main"]);
    assert_eq!(listed(s.get("/orgs/acme/projects").await), ["billing/main", "customer-service/main"]);
    assert_eq!(listed(s.get("/orgs/nobody/projects").await), Vec::<String>::new(), "an org with nothing deployed has nothing");
    assert_eq!(
        s.get("/orgs").await.body["orgs"],
        json!([{ "org": "acme", "deployments": 2 }, { "org": "default", "deployments": 1 }, { "org": "globex", "deployments": 1 }])
    );

    // What one org has, another does not, and is told so by name.
    let a = s.get("/orgs/globex/projects/billing/branches/main").await;
    assert_eq!((a.status, a.class()), (StatusCode::NOT_FOUND, "NotFound"));
    assert!(a.error().ends_with("in org globex"), "{}", a.error());
    assert_eq!(s.get("/projects/billing/branches/main").await.status, StatusCode::NOT_FOUND);

    // An org is a directory's name: nothing that could leave the data
    // directory, reach the server's own, or spell an org two ways.
    for path in ["/orgs/Acme/projects", "/orgs/.cache/projects", "/orgs/..%2Fdefault/projects", "/orgs/a.b/projects"] {
        let a = s.get(path).await;
        assert_eq!((a.status, a.class()), (StatusCode::BAD_REQUEST, "Invalid"), "{path}: {}", a.error());
    }
    let a = s.deploy_to("..%2F..", CS, "main", archive(CS, "evil")).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.error());
    // The server's lock is a file where an org of that name would be:
    // refused as a name, not as a failure of the disk.
    let a = s.deploy_to("lock", CS, "main", archive(CS, "lock")).await;
    assert_eq!((a.status, a.class()), (StatusCode::BAD_REQUEST, "Invalid"), "{}", a.error());
    // What was removed is named with its org, unless that is the default.
    assert_eq!(s.deploy_to("passing", CS, "main", archive(CS, "passing")).await.status, StatusCode::OK);
    let a = s.send(Method::DELETE, &format!("/orgs/passing/projects/{CS}/branches/main"), &[], None).await;
    assert_eq!(a.body["removed"], format!("passing/{CS}/main"), "{}", a.body);

    // A route under an org takes the body its twin takes: an archive
    // may be larger than any JSON request, and is refused for what it
    // is, where a JSON body of that size is refused for its size.
    let big = vec![0u8; 2 * 1024 * 1024];
    let a = s.deploy_to("acme", CS, "big", big.clone()).await;
    assert_eq!((a.status, a.class()), (StatusCode::BAD_REQUEST, "DeployError"), "{}", a.error());
    let a = s.send(Method::POST, &format!("/orgs/acme/projects/{CS}/branches/main/sql"), &[], Some(("application/json", big))).await;
    assert_eq!(a.status, StatusCode::PAYLOAD_TOO_LARGE, "{}", a.error());

    let a = s.send(Method::DELETE, &format!("/orgs/acme/projects/{CS}/branches/main"), &[], None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert_eq!(s.on_disk(), ["acme/billing/main", "default/customer-service/main", "globex/customer-service/main"]);

    // A restart finds each under its org.
    let again = app_over(&s.data_dir);
    assert_eq!(reload(&again).unwrap(), (3, vec![]));
    let mut keys: Vec<String> = again.models.read().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, s.on_disk());
}

/// An org is what its directory is called. `default` renamed while the
/// server is down is that org when it starts again, and what was
/// deployed with no org is the new org's. A deployment laid out as
/// `<project>/<branch>`, from before there were orgs, is neither moved
/// nor served: it is said.
#[tokio::test]
async fn a_renamed_directory_is_that_org_and_an_old_layout_is_said() {
    let s = server();
    assert_eq!(s.deploy(CS, "main", archive(CS, "one")).await.status, StatusCode::OK);
    assert_eq!(s.deploy("billing", "main", archive("billing", "two")).await.status, StatusCode::OK);
    // billing as an old data directory held it, and default renamed.
    std::fs::rename(s.data_dir.join("default").join("billing"), s.data_dir.join("billing")).unwrap();
    std::fs::rename(s.data_dir.join("default"), s.data_dir.join("acme")).unwrap();
    // And a directory no org could be called: said too, and left alone.
    std::fs::create_dir_all(s.data_dir.join("Bad.Name").join(CS).join("main")).unwrap();

    let again = app_over(&s.data_dir);
    let (n, skipped) = reload(&again).unwrap();
    assert_eq!((n, skipped), (1, vec!["Bad.Name".to_string(), "billing/main".to_string()]));
    let keys: Vec<String> = again.models.read().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["acme/customer-service/main"]);
    assert!(s.data_dir.join("billing").join("main").join("snapshot.json.gz").exists(), "nothing is moved");

    let renamed = Server { router: router(again.clone()), app: again, data_dir: s.data_dir.clone() };
    assert_eq!(renamed.get(&format!("/orgs/acme/projects/{CS}/branches/main")).await.status, StatusCode::OK);
    assert_eq!(renamed.get(&format!("/projects/{CS}/branches/main")).await.status, StatusCode::NOT_FOUND);
    // And default is somewhere to deploy to again.
    assert_eq!(renamed.deploy(CS, "main", archive(CS, "new")).await.status, StatusCode::OK);
    assert_eq!(renamed.on_disk(), ["acme/customer-service/main", "default/customer-service/main"]);
    std::mem::forget(renamed);
}

/// An agent is connected for one org: the catalog it is handed, the
/// projects it lists and the branch a tool falls back to are that
/// org's, and no tool takes an org.
#[tokio::test]
async fn an_orgs_agent_is_told_of_no_other_org() {
    let s = server();
    assert_eq!(s.deploy(CS, "main", archive(CS, "plain")).await.status, StatusCode::OK);
    assert_eq!(s.deploy(CS, "dev", archive(CS, "plain-dev")).await.status, StatusCode::OK);
    assert_eq!(s.deploy_to("acme", CS, "dev", archive(CS, "acme")).await.status, StatusCode::OK);
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } });
    let told = |a: Answer| a.body["result"]["instructions"].as_str().unwrap().to_string();

    let acme = told(s.send(Method::POST, "/orgs/acme/mcp", &[], Some(("application/json", serde_json::to_vec(&init).unwrap()))).await);
    assert!(acme.contains("Marker acme") && !acme.contains("Marker plain"), "{acme}");
    let plain = told(s.mcp(&[], init).await);
    assert!(plain.contains("Marker plain") && !plain.contains("Marker acme"), "{plain}");

    let a = s.tool_of("acme", "list_projects", json!({})).await;
    let listed = a.body["result"]["structuredContent"]["deployments"].as_array().unwrap();
    assert_eq!((listed.len(), &listed[0]["branch"]), (1, &json!("dev")));
    // acme has one branch of the project, so that is the one meant;
    // default has two and falls back to the production branch.
    let a = s.tool_of("acme", "fields", json!({ "project": CS, "q": "marker" })).await;
    assert!(a.body["result"]["content"][0]["text"].as_str().unwrap().contains("Marker acme"), "{}", a.body);
    let a = s.tool(&[], "fields", json!({ "project": CS, "q": "marker" })).await;
    let text = a.body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Marker plain") && !text.contains("Marker plain-dev") && !text.contains("Marker acme"), "{text}");
    // An org is not a tool's to name.
    let a = s.tool(&[], "fields", json!({ "project": CS, "org": "acme" })).await;
    assert!(a.body["error"].is_object(), "{}", a.body);
    let a = s.tool_of("nobody", "fields", json!({ "project": CS })).await;
    assert_eq!(a.body["result"]["isError"], true, "{}", a.body);
}

/// A result is its org's alone. The same question in two orgs is two
/// runs; a result asked for or cancelled from another org is not
/// there, in the words an unknown id gets; and an invalidation reaches
/// one org's results.
#[tokio::test]
async fn a_result_is_its_orgs_alone() {
    let s = server();
    assert_eq!(s.deploy(CS, "main", archive(CS, "one")).await.status, StatusCode::OK);
    assert_eq!(s.deploy_to("acme", CS, "main", archive(CS, "one")).await.status, StatusCode::OK);
    let file = sales("orgs");
    let ask = || json!({ "expr": "category asc, revenue", "connection": { "file": file.to_str().unwrap() } });
    let run = format!("projects/{CS}/branches/main/run");

    let theirs = s.post(&format!("/orgs/acme/{run}"), ask()).await;
    assert_eq!((&theirs.body["status"], &theirs.body["org_uid"]), (&json!("succeeded"), &json!("acme")), "{}", theirs.body);
    let ours = s.post(&format!("/{run}"), ask()).await;
    assert_eq!((&ours.body["cache"], &ours.body["org_uid"]), (&json!("miss"), &json!("default")), "another org's result answers nothing");
    assert_ne!(ours.body["id"], theirs.body["id"]);
    assert_eq!(s.post(&format!("/orgs/acme/{run}"), ask()).await.body["cache"], "hit");
    let (theirs, ours) = (theirs.body["id"].as_str().unwrap().to_string(), ours.body["id"].as_str().unwrap().to_string());

    assert_eq!(s.get(&format!("/orgs/acme/results/{theirs}")).await.status, StatusCode::OK);
    assert_eq!(s.get(&format!("/results/{ours}")).await.status, StatusCode::OK);
    let unknown = s.get("/results/r-none").await;
    for path in [format!("/results/{theirs}"), format!("/orgs/globex/results/{theirs}"), format!("/orgs/acme/results/{ours}")] {
        let a = s.get(&path).await;
        assert_eq!((a.status, a.class()), (StatusCode::NOT_FOUND, "NotFound"), "{path}");
        assert_eq!(a.error().replace(&theirs, "X").replace(&ours, "X"), unknown.error().replace("r-none", "X"), "{path}");
        let a = s.send(Method::DELETE, &path, &[], None).await;
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(s.get(&format!("/orgs/acme/results/{theirs}")).await.body["status"], "succeeded", "and it is still its org's to read");

    // What changed in one org's warehouse is said for that org.
    let a = s.post("/cache/invalidate", json!({ "datasource": "dw", "tables": ["sales"] })).await;
    assert_eq!(a.body["invalidated"], 1, "{}", a.body);
    assert_eq!(s.post(&format!("/orgs/acme/{run}"), ask()).await.body["cache"], "hit");
    assert_eq!(s.post(&format!("/{run}"), ask()).await.body["cache"], "miss");
    let a = s.post("/orgs/acme/cache/invalidate", json!({ "datasource": "dw", "tables": ["sales"] })).await;
    assert_eq!(a.body["invalidated"], 1, "{}", a.body);
}

/// A deploy carries a datasource's secrets, and they are its credential
/// on that branch: kept apart from the compiled model, never served,
/// used by a run whose request sends none and set aside whole by one
/// that does. A later deploy that carries none keeps them while the
/// datasource points where it did, a new branch starts with none, and
/// a deploy asked to clear, or a removal, takes them away.
#[tokio::test]
async fn a_datasources_credential_is_the_one_it_was_deployed_with() {
    let s = server();
    let file = sales("credentials");
    let entry = |extra: &str| {
        let mut files = fixture_files(CS, "one");
        files.iter_mut().find(|(p, _)| p == "datasources.yml").unwrap().1 =
            format!("dw:\n  name: Warehouse\n  adapter: duckdb\n  tier: hot\n  file: {}\n{extra}", file.display());
        archive_of(&files)
    };
    let with = entry("  username: svc\n  password: deployed-pw\n  oauth_client_secret: deployed-cs\n");
    let without = entry("  username: svc\n");
    // What a run on the branch would connect with: the password and the client secret.
    let connects = |s: &Server, org: &str, branch: &str, sent: Value| {
        let sent = sent.as_object().cloned().unwrap_or_default();
        let (_, secrets) =
            crate::probe::connection_of(&s.app, (org, CS, branch), "dw", None, &sent, None).map_err(|e| e.2).expect("a connection");
        (secrets.password.map(|p| p.expose().to_string()), secrets.oauth_client_secret.map(|p| p.expose().to_string()))
    };
    let both = || (Some("deployed-pw".to_string()), Some("deployed-cs".to_string()));

    let a = s.deploy(CS, "main", with.clone()).await;
    assert_eq!((a.status, &a.body["credentials"]), (StatusCode::OK, &json!({ "dw": "set" })), "{}", a.body);
    assert_eq!(connects(&s, "default", "main", json!({})), both());

    // On disk: in a file of their own that only its owner reads, and
    // nowhere in the compiled model.
    let dir = s.data_dir.join("default").join(CS).join("main");
    let kept = std::fs::read_to_string(dir.join("secrets.json")).unwrap();
    assert!(kept.contains("deployed-pw") && kept.contains("deployed-cs"), "{kept}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dir.join("secrets.json")).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let mut snapshot = String::new();
    std::io::Read::read_to_string(
        &mut flate2::read::GzDecoder::new(std::fs::File::open(dir.join("snapshot.json.gz")).unwrap()),
        &mut snapshot,
    )
    .unwrap();
    assert!(snapshot.contains("\"svc\"") && !snapshot.contains("deployed-"), "the snapshot holds the settings and no secret");

    // Named, never shown; and no answer of a deploy or a run carries one.
    let listed = s.get(&format!("/projects/{CS}/branches/main/datasources")).await;
    assert_eq!(listed.body["datasources"][0]["secrets"], json!(["oauth_client_secret", "password"]));
    assert_eq!(listed.body["datasources"][0]["connection"]["username"], "svc");
    let ran = s.post(&format!("/projects/{CS}/branches/main/run"), json!({ "expr": "category asc, revenue" })).await;
    assert_eq!(ran.body["status"], "succeeded", "{}", ran.body);
    for answer in [&a, &listed, &ran] {
        assert!(!answer.body.to_string().contains("deployed-"), "{}", answer.body);
    }

    // A request that sends any secret is the whole credential: nothing
    // deployed is blended into it. A setting that is no secret is not one.
    assert_eq!(connects(&s, "default", "main", json!({ "password": "theirs" })), (Some("theirs".to_string()), None));
    assert_eq!(connects(&s, "default", "main", json!({ "oauth_token": "t" })), (None, None));
    assert_eq!(connects(&s, "default", "main", json!({ "role": "analyst" })), both());

    // A deploy that carries none keeps it, and a restart finds it.
    let a = s.deploy(CS, "main", without.clone()).await;
    assert_eq!(a.body["credentials"], json!({ "dw": "kept" }), "{}", a.body);
    assert_eq!(connects(&s, "default", "main", json!({})), both());
    let again = app_over(&s.data_dir);
    reload(&again).unwrap();
    let restarted = Server { router: router(again.clone()), app: again, data_dir: s.data_dir.clone() };
    assert_eq!(connects(&restarted, "default", "main", json!({})), both());

    // How it is used may change; where it points may not.
    let a =
        s.deploy(CS, "main", entry("  username: svc\n  schema: marts\n  cache_ttl: 60\n  query_timeout: 45\n  sslmode: require\n")).await;
    assert_eq!(a.body["credentials"], json!({ "dw": "kept" }), "{}", a.body);
    let a = s.deploy(CS, "main", entry("  username: someone_else\n")).await;
    assert_eq!(a.body["credentials"], json!({ "dw": "dropped" }), "{}", a.body);
    assert_eq!(connects(&s, "default", "main", json!({})), (None, None));
    assert!(!dir.join("secrets.json").exists(), "and no file is left for none");
    let a = s.deploy(CS, "main", without.clone()).await;
    assert_eq!(a.body["credentials"], json!({ "dw": "none" }), "once gone it does not come back: {}", a.body);

    // A branch deployed for the first time has none, whatever another
    // branch holds; nor has the same project in another org.
    assert_eq!(s.deploy(CS, "main", with.clone()).await.body["credentials"], json!({ "dw": "set" }));
    let a = s.deploy(CS, "dev", without.clone()).await;
    assert_eq!(a.body["credentials"], json!({ "dw": "none" }), "{}", a.body);
    assert_eq!(connects(&s, "default", "dev", json!({})), (None, None));
    let a = s.deploy_to("acme", CS, "main", without.clone()).await;
    assert_eq!(a.body["credentials"], json!({ "dw": "none" }), "{}", a.body);
    // And an org's own is its own: a named org is served by what it deployed.
    assert_eq!(s.deploy_to("acme", CS, "main", entry("  username: svc\n  password: acme-pw\n")).await.status, StatusCode::OK);
    assert_eq!(connects(&s, "acme", "main", json!({})), (Some("acme-pw".to_string()), None));
    assert_eq!(connects(&s, "default", "main", json!({})), both());

    // A validation keeps nothing and changes nothing.
    let a = s.validate(CS, "main", entry("  username: svc\n  password: validated\n")).await;
    assert!(a.status == StatusCode::OK && a.body["credentials"].is_null(), "{}", a.body);
    assert_eq!(connects(&s, "default", "main", json!({})), both());

    // Asked to clear, a deploy is the whole truth.
    let clear = format!("/projects/{CS}/branches/main/deploy?clear_secrets=true");
    let a = s.send(Method::POST, &clear, &[], Some(("application/gzip", without.clone()))).await;
    assert_eq!((a.status, &a.body["credentials"]), (StatusCode::OK, &json!({ "dw": "dropped" })), "{}", a.body);
    assert_eq!(connects(&s, "default", "main", json!({})), (None, None));
    let typo = format!("/projects/{CS}/branches/main/deploy?clear_secret=true");
    assert_eq!(s.send(Method::POST, &typo, &[], Some(("application/gzip", without))).await.status, StatusCode::BAD_REQUEST);

    // A removal takes the credential with the branch.
    let acme = s.data_dir.join("acme").join(CS).join("main");
    assert!(acme.join("secrets.json").exists());
    assert_eq!(s.send(Method::DELETE, &format!("/orgs/acme/projects/{CS}/branches/main"), &[], None).await.status, StatusCode::OK);
    assert!(!acme.exists());
}

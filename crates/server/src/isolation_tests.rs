//! Tenant and access isolation, end to end.
//!
//! These drive the real router: the guard, authentication, the gate,
//! the tenant key, the models map, the data directory. Every test here
//! is a question of the form "can this caller reach that model?", and
//! the answer has to come out of the whole chain, because the leaks
//! this guards against (one account's listing showing another's
//! deployment, a branch name stepping into another account's
//! directory) each lived in the seams between parts that were fine on
//! their own.
//!
//! Each test gets its own server over a fresh in-memory accounts store
//! and a scratch data directory. The fixture project is small: one
//! table, one dimension, one measure, one planner test, and a marker
//! dimension named for whoever deployed it, so that which model answers
//! can be read off `/fields`.

use super::*;
use axum::body::Body;
use axum::http::{Method, Request};
use tower::ServiceExt;

const PEER: SocketAddr = SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 40000);

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

fn server() -> Server {
    let data_dir = std::env::temp_dir().join(format!("zsqld-isolation-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&data_dir).unwrap();
    let app = app_over(&data_dir);
    Server { router: router(app.clone()), app, data_dir }
}

/// An app over a data directory, as `main` builds one, with the limits
/// off so the tests are about access and not about pace.
fn app_over(data_dir: &std::path::Path) -> Shared {
    let dialects: &'static Dialects = Box::leak(Box::new(Dialects::builtin()));
    Arc::new(App {
        dialects,
        data_dir: data_dir.to_path_buf(),
        models: RwLock::new(HashMap::new()),
        accounts: Some(Store::in_memory().unwrap()),
        signup_open: true,
        secure_cookies: false,
        limits: Limiter::new(limits::Config { mode: limits::Mode::Off, ..limits::Config::default() }),
        admission: Admission::new(&limits::AdmissionCounts::default()),
        caps: Caps { per_project: 5, per_account: 20 },
        mailer: mail::Mailer::new(None, "", Duration::from_secs(20), "http://test").unwrap(),
        mcp: mcp::Settings::builtin(),
        deploying: std::sync::Mutex::new(()),
    })
}

/// A status and a JSON body (null when the body is not JSON).
#[derive(Debug)]
struct Answer {
    status: StatusCode,
    body: Value,
    headers: axum::http::HeaderMap,
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
    async fn send(&self, method: Method, path: &str, key: Option<&str>, body: Option<(&str, Vec<u8>)>) -> Answer {
        self.send_with(method, path, key, &[], body).await
    }

    async fn send_with(
        &self,
        method: Method,
        path: &str,
        key: Option<&str>,
        headers: &[(&str, &str)],
        body: Option<(&str, Vec<u8>)>,
    ) -> Answer {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(k) = key {
            req = req.header("authorization", format!("Bearer {k}"));
        }
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
        let mut req = req.body(body).unwrap();
        req.extensions_mut().insert(ConnectInfo(PEER));
        let res = self.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), 16 << 20).await.unwrap();
        Answer { status, body: serde_json::from_slice(&bytes).unwrap_or(Value::Null), headers }
    }

    /// One JSON-RPC message to the MCP endpoint, with any extra headers.
    async fn mcp(&self, key: Option<&str>, headers: &[(&str, &str)], msg: Value) -> Answer {
        self.send_with(Method::POST, "/mcp", key, headers, Some(("application/json", serde_json::to_vec(&msg).unwrap()))).await
    }

    /// A tool call; answers the JSON-RPC envelope.
    async fn tool(&self, key: Option<&str>, name: &str, args: Value) -> Answer {
        self.mcp(key, &[], json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } }))
            .await
    }

    /// The names a caller's `list_projects` tool answers, as `project/branch`.
    async fn mcp_listing(&self, key: &str) -> Vec<String> {
        let a = self.tool(Some(key), "list_projects", json!({})).await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.body);
        let r = &a.body["result"];
        assert!(r["isError"].is_null(), "{r}");
        r["structuredContent"]["deployments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| format!("{}/{}", d["project"].as_str().unwrap(), d["branch"].as_str().unwrap()))
            .collect()
    }

    /// A session cookie for the web app, as a browser would hold it.
    async fn login_cookie(&self, email: &str, password: &str) -> String {
        let a = self.post("/login", None, json!({ "email": email, "password": password })).await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.error());
        let set = a.headers.get("set-cookie").unwrap().to_str().unwrap();
        set.split(';').next().unwrap().to_string()
    }

    async fn get(&self, path: &str, key: Option<&str>) -> Answer {
        self.send(Method::GET, path, key, None).await
    }
    async fn delete(&self, path: &str, key: Option<&str>) -> Answer {
        self.send(Method::DELETE, path, key, None).await
    }
    async fn post(&self, path: &str, key: Option<&str>, json: Value) -> Answer {
        self.send(Method::POST, path, key, Some(("application/json", serde_json::to_vec(&json).unwrap()))).await
    }
    async fn patch(&self, path: &str, key: Option<&str>, json: Value) -> Answer {
        self.send(Method::PATCH, path, key, Some(("application/json", serde_json::to_vec(&json).unwrap()))).await
    }
    async fn put(&self, path: &str, key: Option<&str>, json: Value) -> Answer {
        self.send(Method::PUT, path, key, Some(("application/json", serde_json::to_vec(&json).unwrap()))).await
    }
    async fn deploy(&self, uid: &str, branch: &str, key: &str, archive: Vec<u8>) -> Answer {
        self.send(Method::POST, &format!("/projects/{uid}/branches/{branch}/deploy"), Some(key), Some(("application/gzip", archive))).await
    }
    async fn validate(&self, uid: &str, branch: &str, key: &str, archive: Vec<u8>) -> Answer {
        self.send(Method::POST, &format!("/projects/{uid}/branches/{branch}/validate"), Some(key), Some(("application/gzip", archive)))
            .await
    }

    /// A new account; answers the admin's personal key.
    async fn signup(&self, account: &str, email: &str, password: &str) -> String {
        let a = self.post("/signup", None, json!({ "account": account, "email": email, "name": email, "password": password })).await;
        assert_eq!(a.status, StatusCode::OK, "signup {account}: {}", a.error());
        a.body["secret"].as_str().unwrap().to_string()
    }

    /// Adds a member and redeems their invite; answers their personal key.
    async fn member(&self, admin: &str, email: &str, role: &str) -> String {
        let a = self.post("/account/members", Some(admin), json!({ "email": email, "role": role })).await;
        assert_eq!(a.status, StatusCode::OK, "add member: {}", a.error());
        let token = a.body["invite"].as_str().expect("a newcomer gets an invite").to_string();
        let a = self.post("/invites/accept", None, json!({ "token": token, "password": "memberpassword1" })).await;
        assert_eq!(a.status, StatusCode::OK, "accept invite: {}", a.error());
        a.body["secret"].as_str().unwrap().to_string()
    }

    /// A query key with no grants yet; answers (id, secret).
    async fn query_key(&self, admin: &str) -> (i64, String) {
        let a = self.post("/account/query-keys", Some(admin), json!({ "name": "app" })).await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.error());
        (a.body["key"]["id"].as_i64().unwrap(), a.body["secret"].as_str().unwrap().to_string())
    }

    /// The dimension names a caller sees on a branch, or the error.
    async fn fields(&self, uid: &str, branch: &str, key: Option<&str>) -> Result<Vec<String>, Answer> {
        let a = self.get(&format!("/projects/{uid}/branches/{branch}/fields?hidden=true"), key).await;
        if a.status != StatusCode::OK {
            return Err(a);
        }
        Ok(a.body["fields"].as_array().unwrap().iter().filter_map(|f| f["name"].as_str().map(str::to_string)).collect())
    }

    /// What the caller's listing holds, as `project/branch`.
    async fn listing(&self, key: &str) -> Vec<String> {
        let a = self.get("/projects", Some(key)).await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.error());
        a.body["deployments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| format!("{}/{}", d["project"].as_str().unwrap(), d["branch"].as_str().unwrap()))
            .collect()
    }

    /// Deployment directories on disk, relative to the data directory.
    fn on_disk(&self) -> Vec<String> {
        let mut out = Vec::new();
        for account in std::fs::read_dir(&self.data_dir).unwrap().flatten().filter(|e| e.path().is_dir()) {
            for project in std::fs::read_dir(account.path()).unwrap().flatten().filter(|e| e.path().is_dir()) {
                for branch in std::fs::read_dir(project.path()).unwrap().flatten().filter(|e| e.path().is_dir()) {
                    if branch.path().join("snapshot.json.gz").exists() {
                        out.push(format!(
                            "{}/{}/{}",
                            account.file_name().to_string_lossy(),
                            project.file_name().to_string_lossy(),
                            branch.file_name().to_string_lossy()
                        ));
                    }
                }
            }
        }
        out.sort();
        out
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

/// The spec a shorthand line makes, for the tools, which take specs only.
fn spec_of(expr: &str) -> Value {
    zsql_shorthand::parse_line(expr).unwrap().0
}

// ---------------------------------------------------------------- tests

/// The case that was found in production: two accounts, one project
/// name. Each must see, query, remove and delete only its own.
#[tokio::test]
async fn two_accounts_with_the_same_project_name_never_see_each_other() {
    let s = server();
    let ann = s.signup("acme", "ann@acme.test", "annpassword1").await;
    let ned = s.signup("newco", "ned@newco.test", "nedpassword1").await;

    let a = s.deploy(CS, "main", &ann, archive(CS, "acme")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s.deploy(CS, "main", &ned, archive(CS, "newco")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());

    // One row each, their own.
    assert_eq!(s.listing(&ann).await, ["customer-service/main"]);
    assert_eq!(s.listing(&ned).await, ["customer-service/main"]);
    assert!(s.fields(CS, "main", Some(&ann)).await.unwrap().contains(&"Marker acme".to_string()));
    assert!(!s.fields(CS, "main", Some(&ann)).await.unwrap().contains(&"Marker newco".to_string()));
    assert!(s.fields(CS, "main", Some(&ned)).await.unwrap().contains(&"Marker newco".to_string()));
    assert!(!s.fields(CS, "main", Some(&ned)).await.unwrap().contains(&"Marker acme".to_string()));

    // Planning goes against the caller's own model. Asking for the
    // other account's field by name gets, at most, a correction to the
    // nearest field of one's own; the other model's column never
    // appears in the SQL.
    let a = s.post(&format!("/projects/{CS}/branches/main/sql"), Some(&ann), json!({ "expr": "Marker acme, Revenue" })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert!(a.body["sql"].as_str().unwrap().contains("marker_acme"), "{}", a.body["sql"]);
    let a = s.post(&format!("/projects/{CS}/branches/main/sql"), Some(&ann), json!({ "expr": "Marker newco, Revenue" })).await;
    let sql = a.body["sql"].as_str().unwrap_or("");
    assert!(!sql.contains("marker_newco"), "the other account's column is unreachable: {sql}");
    let a = s.post(&format!("/projects/{CS}/branches/main/sql"), Some(&ned), json!({ "expr": "Marker newco, Revenue" })).await;
    assert!(a.body["sql"].as_str().unwrap_or("").contains("marker_newco"), "{}", a.error());

    // Disk and memory hold one of each, under the account.
    assert_eq!(s.on_disk(), ["acme/customer-service/main", "newco/customer-service/main"]);
    assert_eq!(s.keys_in_memory(), ["acme/customer-service/main", "newco/customer-service/main"]);

    // A redeploy replaces, never adds.
    let a = s.deploy(CS, "main", &ann, archive(CS, "acme-v2")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert_eq!(s.listing(&ann).await, ["customer-service/main"]);
    assert!(s.fields(CS, "main", Some(&ann)).await.unwrap().contains(&"Marker acme-v2".to_string()));
    assert!(s.fields(CS, "main", Some(&ned)).await.unwrap().contains(&"Marker newco".to_string()));

    // Ned removes his branch: Ann's stands.
    let a = s.delete(&format!("/projects/{CS}/branches/main"), Some(&ned)).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert_eq!(s.on_disk(), ["acme/customer-service/main"]);
    assert!(s.fields(CS, "main", Some(&ann)).await.is_ok());
    assert_eq!(s.fields(CS, "main", Some(&ned)).await.unwrap_err().status, StatusCode::NOT_FOUND);

    // Ned deletes his project: Ann's stands.
    let a = s.deploy(CS, "main", &ned, archive(CS, "newco")).await;
    assert_eq!(a.status, StatusCode::OK);
    let a = s.delete(&format!("/projects/{CS}"), Some(&ned)).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert_eq!(s.on_disk(), ["acme/customer-service/main"]);
    assert_eq!(s.keys_in_memory(), ["acme/customer-service/main"]);
    assert!(s.fields(CS, "main", Some(&ann)).await.is_ok());

    // A restart reloads from disk into the same keys.
    let again = app_over(&s.data_dir);
    let (n, skipped) = reload(&again).unwrap();
    assert_eq!((n, skipped.len()), (1, 0));
    let keys: Vec<String> = again.models.read().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["acme/customer-service/main"]);
}

/// Every branch and project route, for a key from another account and
/// for no key at all. The stranger is told the project does not exist,
/// not that it is forbidden: whether a name is taken elsewhere is not
/// theirs to learn.
#[tokio::test]
async fn a_key_from_another_account_finds_nothing() {
    let s = server();
    let ann = s.signup("acme", "ann@acme.test", "annpassword1").await;
    let ned = s.signup("newco", "ned@newco.test", "nedpassword1").await;
    assert_eq!(s.deploy(CS, "main", &ann, archive(CS, "acme")).await.status, StatusCode::OK);

    let b = format!("/projects/{CS}/branches/main");
    let reads: Vec<(Method, String, Option<Value>)> = vec![
        (Method::GET, b.clone(), None),
        (Method::GET, format!("{b}/fields"), None),
        (Method::GET, format!("{b}/tables"), None),
        (Method::GET, format!("{b}/values/Category"), None),
        (Method::GET, format!("{b}/sampling-plan?dims=Category"), None),
        (Method::POST, format!("{b}/sql"), Some(json!({ "expr": "Category" }))),
        (Method::POST, format!("{b}/explain"), Some(json!({ "expr": "Category" }))),
        (Method::POST, format!("{b}/explore"), Some(json!({ "expr": "Category" }))),
        (Method::POST, format!("{b}/test"), Some(json!({}))),
        (Method::DELETE, b.clone(), None),
        (Method::PATCH, format!("/projects/{CS}"), Some(json!({ "name": "Taken" }))),
        (Method::DELETE, format!("/projects/{CS}"), None),
        (Method::GET, format!("/projects/{CS}/access"), None),
        (Method::PUT, format!("/projects/{CS}/access"), Some(json!({ "email": "ned@newco.test", "level": "owner" }))),
    ];
    for (method, path, body) in &reads {
        let body = body.as_ref().map(|j| ("application/json", serde_json::to_vec(j).unwrap()));
        let a = s.send(method.clone(), path, Some(&ned), body.clone()).await;
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{method} {path} for a stranger: {} {}", a.status, a.error());
        assert!(!a.error().to_lowercase().contains("forbidden"), "{method} {path}: a stranger is not told the project exists");
        let a = s.send(method.clone(), path, None, body).await;
        assert_eq!(a.status, StatusCode::UNAUTHORIZED, "{method} {path} with no key");
    }
    // The listing is empty for him, and Ann's model is untouched.
    assert!(s.listing(&ned).await.is_empty());
    assert_eq!(s.on_disk(), ["acme/customer-service/main"]);
    assert!(s.fields(CS, "main", Some(&ann)).await.unwrap().contains(&"Marker acme".to_string()));
}

/// Inside an account, what a member may do follows their level on the
/// project: read by visibility, write by grant, owner for the protected
/// production branch and for access changes.
#[tokio::test]
async fn members_reach_only_what_their_level_allows() {
    let s = server();
    let ann = s.signup("acme", "ann@acme.test", "annpassword1").await;
    let bob = s.member(&ann, "bob@acme.test", "developer").await;
    assert_eq!(s.deploy("sales", "main", &ann, archive("sales", "sales")).await.status, StatusCode::OK);
    let b = "/projects/sales/branches/main";

    // Visible to the account: Bob reads, cannot change.
    assert!(s.fields("sales", "main", Some(&bob)).await.is_ok());
    assert_eq!(s.listing(&bob).await, ["sales/main"]);
    assert_eq!(s.deploy("sales", "feature", &bob, archive("sales", "bob")).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.delete(b, Some(&bob)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.post(&format!("{b}/test"), Some(&bob), json!({})).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.patch("/projects/sales", Some(&bob), json!({ "name": "Mine" })).await.status, StatusCode::FORBIDDEN);
    assert_eq!(
        s.put("/projects/sales/access", Some(&bob), json!({ "email": "bob@acme.test", "level": "owner" })).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(s.delete("/projects/sales", Some(&bob)).await.status, StatusCode::FORBIDDEN);

    // Restricted: Bob sees nothing until granted, and the listing agrees.
    assert_eq!(s.patch("/projects/sales", Some(&ann), json!({ "visibility": "restricted" })).await.status, StatusCode::OK);
    assert_eq!(s.fields("sales", "main", Some(&bob)).await.unwrap_err().status, StatusCode::FORBIDDEN);
    assert!(s.listing(&bob).await.is_empty());
    assert_eq!(
        s.put("/projects/sales/access", Some(&ann), json!({ "email": "bob@acme.test", "level": "read" })).await.status,
        StatusCode::OK
    );
    assert!(s.fields("sales", "main", Some(&bob)).await.is_ok());
    assert_eq!(s.listing(&bob).await, ["sales/main"]);

    // Write: a feature branch, yes; the protected production branch, no.
    assert_eq!(
        s.put("/projects/sales/access", Some(&ann), json!({ "email": "bob@acme.test", "level": "write" })).await.status,
        StatusCode::OK
    );
    assert_eq!(s.patch("/projects/sales", Some(&ann), json!({ "protected_production": true })).await.status, StatusCode::OK);
    let a = s.deploy("sales", "feature", &bob, archive("sales", "bob")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s.deploy("sales", "main", &bob, archive("sales", "bob")).await;
    assert_eq!(a.status, StatusCode::FORBIDDEN, "{}", a.error());
    assert!(a.error().contains("needs an owner"));
    assert!(s.fields("sales", "main", Some(&ann)).await.unwrap().contains(&"Marker sales".to_string()), "production is as Ann left it");

    // Owner: production too, and access.
    assert_eq!(
        s.put("/projects/sales/access", Some(&ann), json!({ "email": "bob@acme.test", "level": "owner" })).await.status,
        StatusCode::OK
    );
    assert_eq!(s.deploy("sales", "main", &bob, archive("sales", "bob")).await.status, StatusCode::OK);

    // Removed from the account, Bob's key is no key at all.
    assert_eq!(s.delete("/account/members/bob@acme.test", Some(&ann)).await.status, StatusCode::OK);
    assert_eq!(s.get("/me", Some(&bob)).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(s.fields("sales", "main", Some(&bob)).await.unwrap_err().status, StatusCode::UNAUTHORIZED);
}

/// A query key reads exactly the project and branch it is granted, in
/// its own account, and nothing else anywhere.
#[tokio::test]
async fn query_keys_read_exactly_what_they_are_granted() {
    let s = server();
    let ann = s.signup("acme", "ann@acme.test", "annpassword1").await;
    let ned = s.signup("newco", "ned@newco.test", "nedpassword1").await;
    assert_eq!(s.deploy(CS, "main", &ann, archive(CS, "acme")).await.status, StatusCode::OK);
    assert_eq!(s.deploy(CS, "staging", &ann, archive(CS, "acme-staging")).await.status, StatusCode::OK);
    assert_eq!(s.deploy("hr", "main", &ann, archive("hr", "hr")).await.status, StatusCode::OK);
    assert_eq!(s.deploy(CS, "main", &ned, archive(CS, "newco")).await.status, StatusCode::OK);
    let (id, qk) = s.query_key(&ann).await;
    let grants = format!("/account/query-keys/{id}/grants");

    // Nothing granted: nothing readable, and the listing is empty.
    assert_eq!(s.fields(CS, "main", Some(&qk)).await.unwrap_err().status, StatusCode::FORBIDDEN);
    assert!(s.listing(&qk).await.is_empty());

    // The production branch of one project.
    assert_eq!(s.post(&grants, Some(&ann), json!({ "project": CS })).await.status, StatusCode::OK);
    assert!(s.fields(CS, "main", Some(&qk)).await.unwrap().contains(&"Marker acme".to_string()));
    assert_eq!(s.fields(CS, "staging", Some(&qk)).await.unwrap_err().status, StatusCode::FORBIDDEN);
    assert_eq!(s.fields("hr", "main", Some(&qk)).await.unwrap_err().status, StatusCode::FORBIDDEN);
    assert_eq!(s.listing(&qk).await, ["customer-service/main"]);
    let a = s.post(&format!("/projects/{CS}/branches/main/sql"), Some(&qk), json!({ "expr": "Marker acme, Revenue" })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());

    // One named branch; then every branch.
    assert_eq!(s.post(&grants, Some(&ann), json!({ "project": CS, "branch": "staging" })).await.status, StatusCode::OK);
    assert_eq!(s.fields(CS, "main", Some(&qk)).await.unwrap_err().status, StatusCode::FORBIDDEN);
    assert!(s.fields(CS, "staging", Some(&qk)).await.unwrap().contains(&"Marker acme-staging".to_string()));
    assert_eq!(s.post(&grants, Some(&ann), json!({ "project": CS, "branch": "*" })).await.status, StatusCode::OK);
    assert!(s.fields(CS, "main", Some(&qk)).await.is_ok());
    assert!(s.fields(CS, "staging", Some(&qk)).await.is_ok());
    assert_eq!(s.listing(&qk).await, ["customer-service/main", "customer-service/staging"]);

    // Read only, and never the account's management routes.
    assert_eq!(s.deploy(CS, "main", &qk, archive(CS, "qk")).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.delete(&format!("/projects/{CS}/branches/main"), Some(&qk)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.post(&format!("/projects/{CS}/branches/main/test"), Some(&qk), json!({})).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.get("/account/keys", Some(&qk)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.get("/account/members", Some(&qk)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.get(&format!("/projects/{CS}/access"), Some(&qk)).await.status, StatusCode::FORBIDDEN);

    // Ned cannot grant Ann's key, nor see it; his own key reads his own model.
    assert_eq!(s.post(&grants, Some(&ned), json!({ "project": CS })).await.status, StatusCode::NOT_FOUND);
    assert_eq!(s.delete(&format!("/account/query-keys/{id}"), Some(&ned)).await.status, StatusCode::NOT_FOUND);
    let (nid, nqk) = s.query_key(&ned).await;
    assert_eq!(
        s.post(&format!("/account/query-keys/{nid}/grants"), Some(&ned), json!({ "project": CS, "branch": "*" })).await.status,
        StatusCode::OK
    );
    assert!(s.fields(CS, "main", Some(&nqk)).await.unwrap().contains(&"Marker newco".to_string()));
    assert_eq!(
        s.fields(CS, "staging", Some(&nqk)).await.unwrap_err().status,
        StatusCode::NOT_FOUND,
        "Ann's staging does not exist for Newco"
    );
    assert_eq!(s.listing(&nqk).await, ["customer-service/main"]);

    // Revoked grant, rotated secret.
    assert_eq!(s.delete(&format!("{grants}/{CS}"), Some(&ann)).await.status, StatusCode::OK);
    assert_eq!(s.fields(CS, "main", Some(&qk)).await.unwrap_err().status, StatusCode::FORBIDDEN);
    let a = s.post(&format!("/account/query-keys/{id}/rotate"), Some(&ann), json!({})).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert_eq!(s.get("/me", Some(&qk)).await.status, StatusCode::UNAUTHORIZED, "the old secret is dead");
}

/// A personal key acts in the account that issued it. The same person
/// in two accounts has two keys, and each reaches one account's models.
#[tokio::test]
async fn one_person_in_two_accounts_has_a_key_for_each() {
    let s = server();
    let ann = s.signup("acme", "ann@acme.test", "annpassword1").await;
    let ned = s.signup("newco", "ned@newco.test", "nedpassword1").await;
    assert_eq!(s.deploy(CS, "main", &ann, archive(CS, "acme")).await.status, StatusCode::OK);
    assert_eq!(s.deploy(CS, "main", &ned, archive(CS, "newco")).await.status, StatusCode::OK);

    // Ned invites Ann into Newco as an admin. She already signs in, so the
    // invite cannot mint her a key; she makes one from a Newco session.
    let a = s.post("/account/members", Some(&ned), json!({ "email": "ann@acme.test", "role": "admin" })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert!(a.body.get("invite").is_none(), "someone who can sign in gets no invite token");
    assert_eq!(s.get("/me", Some(&ann)).await.body["account"]["uid"], "acme", "her Acme key still answers for Acme");
    assert!(s.fields(CS, "main", Some(&ann)).await.unwrap().contains(&"Marker acme".to_string()));

    // Her Newco key, made by Newco's admin on her behalf through the
    // store, acts in Newco; her Acme key still acts in Acme.
    let store = s.app.accounts.as_ref().unwrap();
    let ann_user = store.verify_password("ann@acme.test", "annpassword1").unwrap().unwrap();
    let newco = s.get("/me", Some(&ned)).await.body["account"]["id"].as_i64().unwrap();
    let (_, ann_newco) = store.create_user_key(newco, &ann_user, "newco laptop").unwrap();
    assert_eq!(s.get("/me", Some(&ann_newco)).await.body["account"]["uid"], "newco");
    assert!(s.fields(CS, "main", Some(&ann_newco)).await.unwrap().contains(&"Marker newco".to_string()));
    assert!(s.fields(CS, "main", Some(&ann)).await.unwrap().contains(&"Marker acme".to_string()));
    assert_eq!(s.listing(&ann_newco).await, ["customer-service/main"]);

    // Out of Newco: that key dies, the Acme one does not.
    assert_eq!(s.delete("/account/members/ann@acme.test", Some(&ned)).await.status, StatusCode::OK);
    assert_eq!(s.get("/me", Some(&ann_newco)).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(s.get("/me", Some(&ann)).await.body["account"]["uid"], "acme");
}

/// Names that could step out of the account's slice of the data
/// directory are refused before anything is read or written, and the
/// other account's deployment is exactly as it was.
#[tokio::test]
async fn names_that_could_leave_the_account_are_refused() {
    let s = server();
    let ann = s.signup("acme", "ann@acme.test", "annpassword1").await;
    let ned = s.signup("newco", "ned@newco.test", "nedpassword1").await;
    assert_eq!(s.deploy(CS, "main", &ann, archive(CS, "acme")).await.status, StatusCode::OK);
    assert_eq!(s.deploy(CS, "main", &ned, archive(CS, "newco")).await.status, StatusCode::OK);
    let newco_snapshot = s.data_dir.join("newco").join(CS).join("main").join("snapshot.json.gz");
    let before = std::fs::read(&newco_snapshot).unwrap();

    let traversal = "..%2F..%2Fnewco%2Fcustomer-service%2Fmain";
    for (method, path) in [
        (Method::POST, format!("/projects/{CS}/branches/{traversal}/deploy")),
        (Method::POST, format!("/projects/{CS}/branches/{traversal}/validate")),
        (Method::DELETE, format!("/projects/{CS}/branches/{traversal}")),
        (Method::GET, format!("/projects/{CS}/branches/{traversal}/fields")),
        (Method::POST, format!("/projects/..%2Fnewco%2F{CS}/branches/main/deploy")),
        (Method::DELETE, "/projects/..%2Fnewco".to_string()),
        (Method::POST, "/projects/Customer-Service/branches/main/deploy".to_string()),
        (Method::POST, format!("/projects/{CS}/branches/feature%2Fx/deploy")),
        (Method::POST, format!("/projects/{CS}/branches/.hidden/deploy")),
    ] {
        let body = (method == Method::POST).then(|| ("application/gzip", archive(CS, "evil")));
        let a = s.send(method.clone(), &path, Some(&ann), body).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{method} {path}: {} {}", a.status, a.error());
        assert_eq!(a.class(), "Invalid", "{method} {path}");
    }
    assert_eq!(std::fs::read(&newco_snapshot).unwrap(), before, "Newco's deployment is untouched");
    assert_eq!(s.on_disk(), ["acme/customer-service/main", "newco/customer-service/main"]);
    assert_eq!(s.keys_in_memory(), ["acme/customer-service/main", "newco/customer-service/main"]);
    assert!(s.fields(CS, "main", Some(&ned)).await.unwrap().contains(&"Marker newco".to_string()));
}

/// What is inside an archive cannot reach outside it: no links, no
/// imports beyond the project, no server paths in what comes back, and
/// a validate leaves nothing behind.
#[tokio::test]
async fn an_archive_cannot_reach_outside_itself() {
    let s = server();
    let ann = s.signup("acme", "ann@acme.test", "annpassword1").await;
    let ned = s.signup("newco", "ned@newco.test", "nedpassword1").await;
    assert_eq!(s.deploy(CS, "main", &ned, archive(CS, "newco")).await.status, StatusCode::OK);

    // Something on the host that must never be read: here, a file with
    // a marker, standing in for another account's data.
    let outside = s.data_dir.join("outside.yml");
    std::fs::write(
        &outside,
        "fields:\n  - type: dimension\n    name: LEAKED_FROM_HOST\n    data_type: string\n    expression:\n      sql: x\n",
    )
    .unwrap();

    // A symbolic link entry.
    let a = s.validate(CS, "main", &ann, archive_with_link(CS, "tests/leak.yml", outside.to_str().unwrap())).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.error());
    assert!(a.error().contains("not a file or a directory"), "{}", a.error());

    // An import by absolute path.
    let mut files = fixture_files(CS, "importer");
    files[2].1 = format!("imports:\n  - {}\n{}", outside.display(), files[2].1);
    let a = s.validate(CS, "main", &ann, archive_of(&files)).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.error());
    assert!(a.error().contains("Import outside the project"), "{}", a.error());
    assert!(!a.error().contains("LEAKED_FROM_HOST"), "nothing read is shown: {}", a.error());

    // An import by relative path that climbs out. The work directory
    // and this test's data directory are siblings under the temp
    // directory, so from `models/` two steps up and over reaches it.
    let mut files = fixture_files(CS, "climber");
    files[2].1 = format!("imports:\n  - ../../{}/outside.yml\n{}", s.data_dir.file_name().unwrap().to_string_lossy(), files[2].1);
    let a = s.validate(CS, "main", &ann, archive_of(&files)).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.error());
    assert!(a.error().contains("Import outside the project"), "{}", a.error());
    assert!(!a.error().contains("LEAKED_FROM_HOST"), "{}", a.error());

    // A broken project's error names its own files, not the server's.
    let mut files = fixture_files(CS, "broken");
    files[2].1 = "name: Sales\nphysical_name: sales\ndatasource: nowhere\nfields: []\n".into();
    let a = s.validate(CS, "main", &ann, archive_of(&files)).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);
    assert!(!a.error().contains("zsqld-deploy"), "the work directory stays private: {}", a.error());

    // None of that left anything: no project, no files, nothing in memory.
    assert!(s.listing(&ann).await.is_empty());
    assert_eq!(s.on_disk(), ["newco/customer-service/main"]);
    assert_eq!(s.keys_in_memory(), ["newco/customer-service/main"]);
    assert!(s.fields(CS, "main", Some(&ned)).await.unwrap().contains(&"Marker newco".to_string()));
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

// ------------------------------------------------------------------- mcp

/// The endpoint takes a key in the Authorization header and nothing
/// else: no cookie, no key in the query string, no page from another
/// origin, and no stream to hold open.
#[tokio::test]
async fn mcp_takes_a_key_and_nothing_else() {
    let s = server();
    let ann = s.signup("acme", "ann@acme.test", "annpassword1").await;
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "test", "version": "0" } } });

    // No key: the handshake is answered, so a hosted client can see the
    // server is here before its user's key is applied, but it carries no
    // catalogue, and the first tool call is a 401 with the challenge.
    let a = s.mcp(None, &[], init.clone()).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let text = a.body["result"]["instructions"].as_str().unwrap();
    assert!(text.contains("No key was presented") && !text.contains("## Project"), "{text}");
    let a = s.mcp(None, &[], json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/list" })).await;
    assert_eq!(a.status, StatusCode::OK);
    let a = s.tool(None, "list_projects", json!({})).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    assert_eq!(a.headers.get("www-authenticate").unwrap().to_str().unwrap(), "Bearer realm=\"0sql\"");

    // A signed-in browser's cookie, with the header the app sends: the
    // same as no key, since no cookie is read here.
    let cookie = s.login_cookie("ann@acme.test", "annpassword1").await;
    let a = s.tool(None, "list_projects", json!({})).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    let a = s
        .mcp(
            None,
            &[("cookie", &cookie), ("x-requested-with", "zsql")],
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "list_projects", "arguments": {} } }),
        )
        .await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED, "{}", a.error());
    let a = s.get("/me", None).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    let a = s.send_with(Method::GET, "/me", None, &[("cookie", &cookie)], None).await;
    assert_eq!(a.status, StatusCode::OK, "the cookie is good elsewhere: {}", a.error());

    // A key that is not valid.
    let a = s.mcp(Some("zqk_nothing"), &[], init.clone()).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    assert!(a.headers.contains_key("www-authenticate"));

    // A bare key in the header, with no scheme, is taken; another scheme is not.
    let a = s
        .mcp(
            None,
            &[("authorization", &ann)],
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "list_projects", "arguments": {} } }),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let a = s.mcp(None, &[("authorization", &format!("Basic {ann}"))], init.clone()).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    let a = s.mcp(None, &[("authorization", &format!("BEARER {ann}"))], init.clone()).await;
    assert_eq!(a.status, StatusCode::OK, "the scheme is matched without regard to case: {}", a.body);

    // The key: initialize answers the version asked for, and names the tools.
    let a = s.mcp(Some(&ann), &[], init.clone()).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(a.body["result"]["serverInfo"]["name"], "0sql");
    let a = s
        .mcp(Some(&ann), &[], json!({ "jsonrpc": "2.0", "id": 2, "method": "initialize", "params": { "protocolVersion": "1999-01-01" } }))
        .await;
    assert_eq!(a.body["result"]["protocolVersion"], "2025-11-25", "an unknown version gets the newest");

    // Origin is not read: a page from anywhere gets what its key allows,
    // and without one it gets the 401, because nothing here is a cookie.
    let a = s.mcp(Some(&ann), &[("origin", "https://claude.ai")], init.clone()).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let a = s
        .mcp(
            None,
            &[("origin", "https://evil.example")],
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "fields", "arguments": { "project": CS } } }),
        )
        .await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);

    // OAuth discovery finds nothing, plainly: a 404, not a request for a key.
    let a = s.get("/.well-known/oauth-protected-resource", None).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);
    let a = s.get("/.well-known/oauth-authorization-server", None).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);

    // No stream: GET and DELETE are not offered.
    let a = s.get("/mcp", Some(&ann)).await;
    assert_eq!(a.status, StatusCode::METHOD_NOT_ALLOWED);
    let a = s.delete("/mcp", Some(&ann)).await;
    assert_eq!(a.status, StatusCode::METHOD_NOT_ALLOWED);

    // The wire: a notification is taken, a batch is not, an unknown
    // method and an unsupported version header are named.
    let a = s.mcp(Some(&ann), &[], json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;
    assert_eq!(a.status, StatusCode::ACCEPTED);
    let a = s.mcp(Some(&ann), &[], json!([init.clone()])).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);
    let a = s.mcp(Some(&ann), &[], json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list" })).await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(a.body["error"]["code"], -32601);
    let a = s.mcp(Some(&ann), &[("mcp-protocol-version", "2024-11-05")], init.clone()).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);
    let a = s.mcp(Some(&ann), &[("mcp-protocol-version", "2025-11-25")], json!({ "jsonrpc": "2.0", "id": 4, "method": "ping" })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["result"], json!({}));
}

/// Through the tools a key sees and plans against exactly the branches
/// it may read, and no tool takes a context.
#[tokio::test]
async fn mcp_tools_reach_what_the_key_may_read_and_nothing_else() {
    let s = server();
    let ann = s.signup("acme", "ann@acme.test", "annpassword1").await;
    let ned = s.signup("newco", "ned@newco.test", "nedpassword1").await;
    assert_eq!(s.deploy(CS, "main", &ann, archive(CS, "acme")).await.status, StatusCode::OK);
    assert_eq!(s.deploy(CS, "dev", &ann, archive(CS, "acme-dev")).await.status, StatusCode::OK);
    assert_eq!(s.deploy("billing", "main", &ann, archive("billing", "acme-billing")).await.status, StatusCode::OK);
    assert_eq!(s.deploy(CS, "main", &ned, archive(CS, "newco")).await.status, StatusCode::OK);

    // The listed tools, and not one of them takes a context.
    let a = s.mcp(Some(&ann), &[], json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })).await;
    let tools = a.body["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["list_projects", "fields", "tables", "values", "explore", "sql", "explain"]);
    for t in tools {
        assert!(t["inputSchema"]["properties"]["context"].is_null(), "{}", t["name"]);
        assert_eq!(t["inputSchema"]["additionalProperties"], false, "{}", t["name"]);
    }

    // A query key on one branch of one project.
    let (qk, secret) = s.query_key(&ann).await;
    let a = s.post(&format!("/account/query-keys/{qk}/grants"), Some(&ann), json!({ "project": CS })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert_eq!(s.mcp_listing(&secret).await, ["customer-service/main"]);
    assert_eq!(s.mcp_listing(&ann).await, ["billing/main", "customer-service/dev", "customer-service/main"]);
    assert_eq!(s.mcp_listing(&ned).await, ["customer-service/main"]);

    // The branch defaults to production; a named one is held to the grant.
    let a = s.tool(Some(&secret), "fields", json!({ "project": CS, "hidden": true })).await;
    let names: Vec<&str> =
        a.body["result"]["structuredContent"]["fields"].as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"Marker acme") && !names.contains(&"Marker newco"), "{names:?}");
    let a = s.tool(Some(&secret), "fields", json!({ "project": CS, "branch": "dev" })).await;
    assert_eq!(a.body["result"]["isError"], true, "{}", a.body);
    assert!(a.body["result"]["content"][0]["text"].as_str().unwrap().contains("not granted"), "{}", a.body);
    let a = s.tool(Some(&secret), "tables", json!({ "project": "billing" })).await;
    assert_eq!(a.body["result"]["isError"], true, "{}", a.body);
    let a = s.tool(Some(&ann), "tables", json!({ "project": "billing" })).await;
    assert!(a.body["result"]["isError"].is_null(), "{}", a.body);

    // Planning: the other account's column is unreachable by any name.
    let a = s.tool(Some(&secret), "sql", json!({ "project": CS, "spec": spec_of("Marker acme, Revenue") })).await;
    let sql = a.body["result"]["structuredContent"]["sql"].as_str().unwrap_or("");
    assert!(sql.contains("marker_acme"), "{}", a.body);
    let a = s.tool(Some(&ned), "sql", json!({ "project": CS, "spec": spec_of("Marker acme, Revenue") })).await;
    let sql = a.body["result"]["structuredContent"]["sql"].as_str().unwrap_or("");
    assert!(!sql.contains("marker_acme"), "{}", a.body);
    let a = s.tool(Some(&ned), "explain", json!({ "project": CS, "spec": spec_of("Marker newco, Revenue") })).await;
    assert!(a.body["result"]["structuredContent"]["sql"].as_str().unwrap().contains("marker_newco"), "{}", a.body);
    assert!(a.body["result"]["structuredContent"]["nodes"].is_array());
    let a = s.tool(Some(&secret), "explore", json!({ "project": CS, "spec": spec_of("Revenue"), "q": "cat" })).await;
    assert!(a.body["result"]["structuredContent"]["dimensions"].as_array().unwrap().iter().any(|d| d["name"] == "Category"), "{}", a.body);

    // Arguments the tools do not take.
    let a = s.tool(Some(&secret), "sql", json!({ "project": CS, "spec": spec_of("Revenue"), "context": { "system_admin": true } })).await;
    assert_eq!(a.body["error"]["code"], -32602, "{}", a.body);
    assert!(a.body["error"]["message"].as_str().unwrap().contains("fixed by the key"));
    let a = s.tool(Some(&secret), "sql", json!({ "project": CS, "spec": spec_of("Revenue"), "limit": 5 })).await;
    assert_eq!(a.body["error"]["code"], -32602, "{}", a.body);
    let a = s.tool(Some(&secret), "deploy", json!({ "project": CS })).await;
    assert_eq!(a.body["error"]["code"], -32602, "{}", a.body);

    // Without a key, nothing of anyone's: the handshake only.
    let a = s.tool(None, "list_projects", json!({})).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    let a = s.tool(None, "fields", json!({ "project": CS })).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);

    // Revoked, the very next call is refused.
    let a = s.delete(&format!("/account/query-keys/{qk}"), Some(&ann)).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s.tool(Some(&secret), "list_projects", json!({})).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
}

/// Who a query key plans as is the project owner's decision, made on
/// the grant, and once made the caller cannot change it on any path.
#[tokio::test]
async fn a_bound_context_decides_who_a_query_key_plans_as() {
    let s = server();
    let ann = s.signup("acme", "ann@acme.test", "annpassword1").await;
    let dev = s.member(&ann, "dev@acme.test", "developer").await;
    assert_eq!(s.deploy(CS, "main", &ann, archive_with_policies(CS, "acme")).await.status, StatusCode::OK);
    let (qk, secret) = s.query_key(&ann).await;
    let sql_path = format!("/projects/{CS}/branches/main/sql");
    let emea = json!({ "groups": [{ "name": "emea" }] });
    let apac = json!({ "groups": [{ "name": "apac" }] });

    // Unbound: REST is as it was, MCP has no context to plan with.
    let a = s.post(&format!("/account/query-keys/{qk}/grants"), Some(&ann), json!({ "project": CS })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s.post(&sql_path, Some(&secret), json!({ "expr": "Revenue" })).await;
    assert_eq!((a.status, a.class()), (StatusCode::BAD_REQUEST, "ContextRequired"), "{}", a.error());
    let a = s.post(&sql_path, Some(&secret), json!({ "expr": "Revenue", "context": emea })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert!(a.body["sql"].as_str().unwrap().contains("emea"), "{}", a.body["sql"]);
    let a = s.tool(Some(&secret), "sql", json!({ "project": CS, "spec": spec_of("Revenue") })).await;
    assert_eq!(a.body["result"]["isError"], true, "{}", a.body);
    assert!(a.body["result"]["content"][0]["text"].as_str().unwrap().starts_with("ContextRequired"), "{}", a.body);

    // A developer who does not own the project cannot bind one.
    let a = s.post(&format!("/account/query-keys/{qk}/grants"), Some(&dev), json!({ "project": CS, "context": apac })).await;
    assert_eq!(a.status, StatusCode::FORBIDDEN, "{}", a.error());
    // Nor can anyone bind something that is not a context.
    let a = s.post(&format!("/account/query-keys/{qk}/grants"), Some(&ann), json!({ "project": CS, "context": { "admin": true } })).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.error());
    let a = s.post(&format!("/account/query-keys/{qk}/grants"), Some(&ann), json!({ "project": CS, "context": "emea" })).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.error());

    // The owner binds apac: MCP plans as apac, REST plans as apac, and a
    // context in a request is refused, the same one included.
    let a = s.post(&format!("/account/query-keys/{qk}/grants"), Some(&ann), json!({ "project": CS, "context": apac })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert_eq!(a.body["grant"]["context"]["groups"][0]["name"], "apac");
    let a = s.tool(Some(&secret), "sql", json!({ "project": CS, "spec": spec_of("Revenue") })).await;
    let sql = a.body["result"]["structuredContent"]["sql"].as_str().unwrap_or("");
    assert!(sql.contains("apac") && !sql.contains("emea"), "{}", a.body);
    let a = s.post(&sql_path, Some(&secret), json!({ "expr": "Revenue" })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert!(a.body["sql"].as_str().unwrap().contains("apac"), "{}", a.body["sql"]);
    let a = s.post(&sql_path, Some(&secret), json!({ "expr": "Revenue", "context": emea })).await;
    assert_eq!((a.status, a.class()), (StatusCode::BAD_REQUEST, "ContextBound"), "{}", a.error());
    let a = s.post(&sql_path, Some(&secret), json!({ "expr": "Revenue", "context": apac })).await;
    assert_eq!(
        (a.status, a.class()),
        (StatusCode::BAD_REQUEST, "ContextBound"),
        "the same context is still the caller's to send: {}",
        a.error()
    );
    let a = s.post(&sql_path, Some(&secret), json!({ "expr": "Revenue", "context": { "system_admin": true } })).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.error());
    let a = s.post(&format!("/projects/{CS}/branches/main/explore"), Some(&secret), json!({ "expr": "Revenue", "context": emea })).await;
    assert_eq!((a.status, a.class()), (StatusCode::BAD_REQUEST, "ContextBound"), "{}", a.error());
    let a = s.post(&format!("/projects/{CS}/branches/main/explore"), Some(&secret), json!({ "expr": "Revenue" })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());

    // The owners see what is bound.
    let a = s.get(&format!("/projects/{CS}/access"), Some(&ann)).await;
    assert_eq!(a.body["query_keys"][0]["grant"]["context"]["groups"][0]["name"], "apac", "{}", a.body);
    let a = s.get("/account/query-keys", Some(&ann)).await;
    assert_eq!(a.body["query_keys"][0]["grants"][0]["context"]["groups"][0]["name"], "apac", "{}", a.body);

    // Rotation keeps it; a grant without one clears it.
    let a = s.post(&format!("/account/query-keys/{qk}/rotate"), Some(&ann), json!({})).await;
    let secret2 = a.body["secret"].as_str().unwrap().to_string();
    let a = s.tool(Some(&secret2), "sql", json!({ "project": CS, "spec": spec_of("Revenue") })).await;
    assert!(a.body["result"]["structuredContent"]["sql"].as_str().unwrap_or("").contains("apac"), "{}", a.body);
    let a = s.post(&format!("/account/query-keys/{qk}/grants"), Some(&ann), json!({ "project": CS, "branch": "*" })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert!(a.body["grant"]["context"].is_null());
    let a = s.post(&sql_path, Some(&secret2), json!({ "expr": "Revenue" })).await;
    assert_eq!((a.status, a.class()), (StatusCode::BAD_REQUEST, "ContextRequired"), "{}", a.error());
    let a = s.post(&sql_path, Some(&secret2), json!({ "expr": "Revenue", "context": emea })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
}

/// A person over MCP is exactly themselves: their email and no flag,
/// whatever their role. Over REST they still choose, as in the playground.
#[tokio::test]
async fn a_personal_key_plans_as_its_user_over_mcp() {
    let s = server();
    let ann = s.signup("acme", "ann@acme.test", "annpassword1").await;
    assert_eq!(s.deploy(CS, "main", &ann, archive_with_policies(CS, "acme")).await.status, StatusCode::OK);

    let a = s.tool(Some(&ann), "sql", json!({ "project": CS, "spec": spec_of("Category") })).await;
    let sql = a.body["result"]["structuredContent"]["sql"].as_str().unwrap_or("");
    assert!(sql.contains("ann@acme.test"), "{}", a.body);
    // The regions policy has no group to resolve for a person: denied,
    // not bypassed, even though Ann is the account's admin.
    let a = s.tool(Some(&ann), "sql", json!({ "project": CS, "spec": spec_of("Revenue") })).await;
    let sql = a.body["result"]["structuredContent"]["sql"].as_str().unwrap_or("");
    assert!(sql.contains("1 = 0"), "{}", a.body);
    let a = s.tool(Some(&ann), "sql", json!({ "project": CS, "spec": spec_of("Revenue"), "context": { "system_admin": true } })).await;
    assert_eq!(a.body["error"]["code"], -32602, "{}", a.body);

    // REST is unchanged for a person.
    let a = s
        .post(&format!("/projects/{CS}/branches/main/sql"), Some(&ann), json!({ "expr": "Revenue", "context": { "system_admin": true } }))
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert!(!a.body["sql"].as_str().unwrap().contains("1 = 0"), "{}", a.body["sql"]);
}

/// A caller's own filter on the policy's context dimension must narrow
/// the rows the policy allows, never replace the policy's filter.
#[tokio::test]
async fn a_user_filter_on_the_context_dimension_cannot_widen_the_policy() {
    let s = server();
    let ann = s.signup("acme", "ann@acme.test", "annpassword1").await;
    assert_eq!(s.deploy(CS, "main", &ann, archive_with_policies(CS, "acme")).await.status, StatusCode::OK);
    let (qk, secret) = s.query_key(&ann).await;
    let a = s
        .post(
            &format!("/account/query-keys/{qk}/grants"),
            Some(&ann),
            json!({ "project": CS, "context": { "groups": [{ "name": "emea" }] } }),
        )
        .await;
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
        let a = s.tool(Some(&secret), "sql", json!({ "project": CS, "spec": spec_of(expr) })).await;
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
    let ann = s.signup("acme", "ann@acme.test", "annpassword1").await;
    assert_eq!(s.deploy(CS, "main", &ann, archive_with_exclusions(CS, "acme")).await.status, StatusCode::OK);
    let (qk, secret) = s.query_key(&ann).await;
    let a = s
        .post(
            &format!("/account/query-keys/{qk}/grants"),
            Some(&ann),
            json!({ "project": CS, "context": { "groups": [{ "name": "emea" }] } }),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    for expr in ["Total Revenue, Region = apac", "Global Revenue, Region = apac", "Peak Region Revenue, Region = apac"] {
        let a = s.tool(Some(&secret), "sql", json!({ "project": CS, "spec": spec_of(expr) })).await;
        let sql = a.body["result"]["structuredContent"]["sql"].as_str().unwrap_or("").to_lowercase();
        assert!(sql.contains("sum(t0.\"revenue\")"), "{expr}: {}", a.body);
        assert!(
            !sql.contains("in ('emea')") && !sql.contains("apac"),
            "{expr}: the measure ignores both the caller's filter and the policy's: {sql}"
        );
    }
    // A caller whose context resolves nothing is still denied.
    let a = s.post(&format!("/account/query-keys/{qk}/grants"), Some(&ann), json!({ "project": CS, "context": {} })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s.tool(Some(&secret), "sql", json!({ "project": CS, "spec": spec_of("Global Revenue") })).await;
    assert!(a.body["result"]["structuredContent"]["sql"].as_str().unwrap_or("").contains("1 = 0"), "{}", a.body);
}

/// What a client is told on connect and by a bare fields call: the
/// catalogue of exactly the branches the key may read, with uids; and a
/// refused field name comes back with the fields nearest it.
#[tokio::test]
async fn the_catalogue_follows_the_key_and_a_refusal_names_the_nearest_fields() {
    let s = server();
    let ann = s.signup("acme", "ann@acme.test", "annpassword1").await;
    assert_eq!(s.deploy(CS, "main", &ann, archive(CS, "acme")).await.status, StatusCode::OK);
    assert_eq!(s.deploy("billing", "main", &ann, archive("billing", "acme-billing")).await.status, StatusCode::OK);
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } });

    // A key with no grants is told so; a key granted one project gets
    // that project's catalogue and not the other's.
    let (qk, secret) = s.query_key(&ann).await;
    let a = s.mcp(Some(&secret), &[], init.clone()).await;
    let text = a.body["result"]["instructions"].as_str().unwrap();
    assert!(text.contains("reads no deployed branch yet"), "{text}");
    let a = s.post(&format!("/account/query-keys/{qk}/grants"), Some(&ann), json!({ "project": CS })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s.mcp(Some(&secret), &[], init.clone()).await;
    let text = a.body["result"]["instructions"].as_str().unwrap();
    assert!(text.contains("## Project customer-service, branch main"), "{text}");
    assert!(text.contains("- revenue: Revenue (decimal)"), "{text}");
    assert!(text.contains("Marker acme (marker-acme)"), "{text}");
    assert!(!text.contains("billing"), "the other project is not this key's to see: {text}");
    // The admin sees both.
    let a = s.mcp(Some(&ann), &[], init).await;
    let text = a.body["result"]["instructions"].as_str().unwrap();
    assert!(text.contains("## Project billing, branch main") && text.contains("## Project customer-service, branch main"), "{text}");

    // fields with no q is the same catalogue as text; with q it is the search.
    let a = s.tool(Some(&secret), "fields", json!({ "project": CS })).await;
    let r = &a.body["result"];
    assert!(r["isError"].is_null(), "{r}");
    assert!(r["content"][0]["text"].as_str().unwrap().starts_with("## Project customer-service, branch main"), "{r}");
    assert!(r["structuredContent"]["catalogue"].is_string());
    let a = s.tool(Some(&secret), "fields", json!({ "project": CS, "q": "revenue" })).await;
    assert_eq!(a.body["result"]["structuredContent"]["fields"][0]["uid"], "revenue", "{}", a.body);

    // A near miss with one clear winner is corrected silently and said
    // so under corrections; one the planner cannot settle between two
    // candidates is refused, and the refusal carries the nearest fields.
    let a = s
        .tool(
            Some(&secret),
            "sql",
            json!({ "project": CS, "spec": { "projections": [{ "field": "Category" }, { "field": "Revenue Grand Total" }] } }),
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
    assert_eq!(s.deploy("billing", "main", &ann, archive_of(&files)).await.status, StatusCode::OK);
    let a = s
        .tool(
            Some(&ann),
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

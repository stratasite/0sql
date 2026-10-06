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
        admission: Admission::from_env().unwrap(),
        caps: Caps { per_project: 5, per_account: 20 },
        mailer: mail::Mailer::from_env("http://test".into()).unwrap(),
    })
}

/// A status and a JSON body (null when the body is not JSON).
#[derive(Debug)]
struct Answer {
    status: StatusCode,
    body: Value,
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
        let mut req = Request::builder().method(method).uri(path);
        if let Some(k) = key {
            req = req.header("authorization", format!("Bearer {k}"));
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
        let bytes = axum::body::to_bytes(res.into_body(), 16 << 20).await.unwrap();
        Answer { status, body: serde_json::from_slice(&bytes).unwrap_or(Value::Null) }
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
        self.send(Method::POST, &format!("/projects/{uid}/branches/{branch}/validate"), Some(key), Some(("application/gzip", archive))).await
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
        a.body["deployments"].as_array().unwrap().iter().map(|d| format!("{}/{}", d["project"].as_str().unwrap(), d["branch"].as_str().unwrap())).collect()
    }

    /// Deployment directories on disk, relative to the data directory.
    fn on_disk(&self) -> Vec<String> {
        let mut out = Vec::new();
        for account in std::fs::read_dir(&self.data_dir).unwrap().flatten().filter(|e| e.path().is_dir()) {
            for project in std::fs::read_dir(account.path()).unwrap().flatten().filter(|e| e.path().is_dir()) {
                for branch in std::fs::read_dir(project.path()).unwrap().flatten().filter(|e| e.path().is_dir()) {
                    if branch.path().join("snapshot.json.gz").exists() {
                        out.push(format!("{}/{}/{}", account.file_name().to_string_lossy(), project.file_name().to_string_lossy(), branch.file_name().to_string_lossy()));
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
    assert_eq!(s.put("/projects/sales/access", Some(&bob), json!({ "email": "bob@acme.test", "level": "owner" })).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.delete("/projects/sales", Some(&bob)).await.status, StatusCode::FORBIDDEN);

    // Restricted: Bob sees nothing until granted, and the listing agrees.
    assert_eq!(s.patch("/projects/sales", Some(&ann), json!({ "visibility": "restricted" })).await.status, StatusCode::OK);
    assert_eq!(s.fields("sales", "main", Some(&bob)).await.unwrap_err().status, StatusCode::FORBIDDEN);
    assert!(s.listing(&bob).await.is_empty());
    assert_eq!(s.put("/projects/sales/access", Some(&ann), json!({ "email": "bob@acme.test", "level": "read" })).await.status, StatusCode::OK);
    assert!(s.fields("sales", "main", Some(&bob)).await.is_ok());
    assert_eq!(s.listing(&bob).await, ["sales/main"]);

    // Write: a feature branch, yes; the protected production branch, no.
    assert_eq!(s.put("/projects/sales/access", Some(&ann), json!({ "email": "bob@acme.test", "level": "write" })).await.status, StatusCode::OK);
    assert_eq!(s.patch("/projects/sales", Some(&ann), json!({ "protected_production": true })).await.status, StatusCode::OK);
    let a = s.deploy("sales", "feature", &bob, archive("sales", "bob")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s.deploy("sales", "main", &bob, archive("sales", "bob")).await;
    assert_eq!(a.status, StatusCode::FORBIDDEN, "{}", a.error());
    assert!(a.error().contains("needs an owner"));
    assert!(s.fields("sales", "main", Some(&ann)).await.unwrap().contains(&"Marker sales".to_string()), "production is as Ann left it");

    // Owner: production too, and access.
    assert_eq!(s.put("/projects/sales/access", Some(&ann), json!({ "email": "bob@acme.test", "level": "owner" })).await.status, StatusCode::OK);
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
    assert_eq!(s.get(&format!("/projects/{CS}/branches/main/sampling-plan?dims=Category"), Some(&qk)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.get("/account/keys", Some(&qk)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.get("/account/members", Some(&qk)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.get(&format!("/projects/{CS}/access"), Some(&qk)).await.status, StatusCode::FORBIDDEN);

    // Ned cannot grant Ann's key, nor see it; his own key reads his own model.
    assert_eq!(s.post(&grants, Some(&ned), json!({ "project": CS })).await.status, StatusCode::NOT_FOUND);
    assert_eq!(s.delete(&format!("/account/query-keys/{id}"), Some(&ned)).await.status, StatusCode::NOT_FOUND);
    let (nid, nqk) = s.query_key(&ned).await;
    assert_eq!(s.post(&format!("/account/query-keys/{nid}/grants"), Some(&ned), json!({ "project": CS, "branch": "*" })).await.status, StatusCode::OK);
    assert!(s.fields(CS, "main", Some(&nqk)).await.unwrap().contains(&"Marker newco".to_string()));
    assert_eq!(s.fields(CS, "staging", Some(&nqk)).await.unwrap_err().status, StatusCode::NOT_FOUND, "Ann's staging does not exist for Newco");
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
    std::fs::write(&outside, "fields:\n  - type: dimension\n    name: LEAKED_FROM_HOST\n    data_type: string\n    expression:\n      sql: x\n").unwrap();

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

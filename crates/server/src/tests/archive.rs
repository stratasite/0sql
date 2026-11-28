//! A deploy kept in the object store, with a directory standing in for
//! one: what the zip is called, what is in it, that no secret is, and
//! that a deploy that cannot be kept is not made.

use super::*;

/// A server whose results, and so its deploys, go to an object store
/// under the data directory.
fn server_with_store() -> (Server, PathBuf) {
    let data_dir = std::env::temp_dir().join(format!("zsqld-tests-{}-{}", std::process::id(), nanos()));
    let store = data_dir.join(".store");
    std::fs::create_dir_all(&store).unwrap();
    let limits = crate::RunLimits { object_store: Some((format!("file://{}", store.display()), vec![])), ..Default::default() };
    let app = app_with(&data_dir, &limits);
    (Server { router: router(app.clone()), app, data_dir }, store)
}

/// Every file of a zip, by name, with a gzipped one read through.
fn unzipped(bytes: Vec<u8>) -> Vec<(String, String)> {
    use std::io::Read;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let names: Vec<String> = zip.file_names().map(|n| n.unwrap().to_string()).collect();
    let mut out = Vec::new();
    for name in names {
        let mut raw = Vec::new();
        zip.by_name(&name).unwrap().read_to_end(&mut raw).unwrap();
        if name.ends_with(".gz") {
            let mut plain = Vec::new();
            flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut plain).unwrap();
            raw = plain;
        }
        out.push((name, String::from_utf8_lossy(&raw).to_string()));
    }
    out.sort();
    out
}

/// The fixture with a datasource deployed the way a careless deployer
/// would: secrets at the top, nested and in a list, and a `.zsql`
/// beside the project.
fn archive_with_secrets(uid: &str) -> Vec<u8> {
    let mut files = fixture_files(uid, "kept");
    files[1].1 = [
        "dw:",
        "  name: Warehouse",
        "  adapter: duckdb",
        "  tier: hot",
        "  host: db.internal",
        "  user: reader",
        "  password: hunter2-top",
        "  oauth:",
        "    client_secret: hunter2-nested",
        "    scope: read",
        "  fallbacks:",
        "    - host: other.internal",
        "      api_key: hunter2-listed",
        "  private_key_file: /keys/dw.p8",
        "  url: https://db.internal/path",
        "token_store:",
        "  name: Tokens",
        "  adapter: duckdb",
        "  host: tokens.internal",
        "",
    ]
    .join("\n");
    files.push((".zsql".into(), "dw:\n  password: hunter2-dotfile\n".into()));
    archive_of(&files)
}

#[tokio::test]
async fn a_deploy_is_kept_in_the_object_store_without_its_secrets() {
    let (s, store) = server_with_store();
    s.make_org("acme").await;
    let a = s.deploy_to("acme", "shop", "main", archive_with_secrets("shop")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);

    // Named for the org, the project, the branch and the moment, in a
    // directory of the branch's own; the answer says where, from the
    // store's own prefix down and no further up.
    let key = a.body["archive"].as_str().expect("the answer says where the deploy was kept").to_string();
    let name = key.strip_prefix("deploys/acme/shop/main/").unwrap_or_else(|| panic!("{key}")).to_string();
    let stamp = name.strip_prefix("acme-shop-main-").and_then(|rest| rest.strip_suffix(".zip")).unwrap_or_else(|| panic!("{key}"));
    assert!(stamp.len() == 20 && stamp.ends_with('Z') && stamp.as_bytes()[8] == b'T' && stamp.as_bytes()[15] == b'.', "{key}");
    let zip_file = store.join(&key);
    let kept: Vec<String> =
        std::fs::read_dir(zip_file.parent().unwrap()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    assert_eq!(kept, vec![name.clone()]);

    let files = unzipped(std::fs::read(&zip_file).unwrap());
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "compiled/snapshot.json.gz",
            "compiled/tests.json",
            "deploy.json",
            "project/datasources.yml",
            "project/models/tbl.sales.yml",
            "project/project.yml",
            "project/tests/smoke.yml",
        ],
        "the files the model was loaded from and what was compiled of them; the .zsql is not one"
    );
    for (name, text) in &files {
        assert!(!text.contains("hunter2"), "{name} holds a secret:\n{text}");
    }
    // What is not a secret is all there, in the source and the snapshot.
    let of = |name: &str| files.iter().find(|(n, _)| n == name).map(|(_, text)| text.clone()).unwrap();
    for name in ["project/datasources.yml", "compiled/snapshot.json.gz"] {
        let text = of(name);
        for kept in ["db.internal", "reader", "other.internal", "scope", "/keys/dw.p8", "https://db.internal/path", "tokens.internal"] {
            assert!(text.contains(kept), "{name} lost {kept}:\n{text}");
        }
        for gone in ["password", "client_secret", "api_key"] {
            assert!(!text.contains(gone), "{name} still names {gone}:\n{text}");
        }
    }
    let said: Value = serde_json::from_str(&of("deploy.json")).unwrap();
    // The whole answer, as it was given: what became of each
    // credential, and where this zip is.
    assert_eq!((&said["archive"], &said["credentials"]), (&json!(key), &a.body["credentials"]), "{said}");
    assert!(said["credentials"].as_object().is_some_and(|c| !c.is_empty()), "{said}");
    assert_eq!(
        (&said["org"], &said["project"], &said["branch"], &said["test_results"]["passed"]),
        (&json!("acme"), &json!("shop"), &json!("main"), &json!(1))
    );

    // The server's own copy has the credential, in the file of secrets
    // beside the snapshot: it is what a run connects with, and that
    // file is the one the copy that leaves is without.
    let held = std::fs::read_to_string(s.data_dir.join("acme/shop/main").join(zsql_core::SECRETS_FILE)).unwrap();
    assert!(held.contains("hunter2-top"), "{held}");

    // The compiled half reads back as a model.
    let back = s.data_dir.join(".back");
    std::fs::create_dir_all(&back).unwrap();
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&zip_file).unwrap()).unwrap();
    for file in ["snapshot.json.gz", "tests.json"] {
        let mut out = std::fs::File::create(back.join(file)).unwrap();
        std::io::copy(&mut zip.by_name(&format!("compiled/{file}")).unwrap(), &mut out).unwrap();
    }
    let model = zsql_core::Model::from_stored(&back, "shop", "main", s.app.dialects).unwrap();
    assert_eq!(model.summary()["fields"], a.body["fields"]);

    // A validation keeps nothing, there or here.
    let body = Some(("application/gzip", archive("shop", "v")));
    let a = s.send(Method::POST, "/orgs/acme/projects/shop/branches/other/validate", &[], body).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert!(a.body.get("archive").is_none());
    assert_eq!(std::fs::read_dir(store.join("deploys/acme/shop")).unwrap().count(), 1);

    // A second deploy is a second zip: the first is still there.
    let b = s.deploy_to("acme", "shop", "main", archive("shop", "again")).await;
    assert_eq!(b.status, StatusCode::OK, "{}", b.body);
    assert_ne!(b.body["archive"], json!(key));
    assert_eq!(std::fs::read_dir(zip_file.parent().unwrap()).unwrap().count(), 2);
}

#[tokio::test]
async fn a_deploy_that_cannot_be_kept_is_not_made() {
    let (s, store) = server_with_store();
    // A file where the store would make a directory: nothing can be put.
    std::fs::write(store.join("deploys"), "in the way").unwrap();
    let a = s.deploy("shop", "main", archive("shop", "a")).await;
    assert_eq!((a.status, a.class()), (StatusCode::BAD_GATEWAY, "ArchiveError"), "{}", a.body);
    assert!(a.error().contains("was not made"), "{}", a.error());
    assert!(s.fields("shop", "main").await.is_err(), "nothing answers for the branch");
    assert!(!s.data_dir.join("default/shop/main").exists());
    let left: Vec<String> = std::fs::read_dir(s.data_dir.join("default/shop"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(left.is_empty(), "nothing staged is left behind: {left:?}");
    assert!(!a.error().contains(&store.display().to_string()), "the answer does not say where the store is: {}", a.error());
}

#[tokio::test]
async fn a_server_with_no_object_store_keeps_a_deploy_only_on_its_disk() {
    let s = server();
    let a = s.deploy("shop", "main", archive("shop", "a")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert!(a.body.get("archive").is_none(), "{}", a.body);
}

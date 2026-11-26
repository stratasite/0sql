//! Datasets through the router: the owner header, the routes, and the
//! `.datasets` directory beside a project's branches.

use super::*;

const BUDGET: &str = "region,month,amount\nEMEA,2026-01-01,100\nAPAC,2026-01-01,80\n";

fn ann() -> (&'static str, &'static str) {
    (crate::OWNER_HEADER, "user-ann")
}

impl Server {
    async fn upload(&self, path: &str, owner: Option<&str>, csv: &str) -> Answer {
        let headers: Vec<(&str, &str)> = owner.map(|o| vec![(crate::OWNER_HEADER, o)]).unwrap_or_default();
        self.send(Method::POST, path, &headers, Some(("text/csv", csv.as_bytes().to_vec()))).await
    }

    async fn as_ann(&self, method: Method, path: &str) -> Answer {
        self.send(method, path, &[ann()], None).await
    }

    fn datasets_dir(&self, org: &str, uid: &str) -> PathBuf {
        self.data_dir.join(org).join(uid).join(zsql_prep::DIR)
    }
}

/// `.datasets` sits beside a project's branches and is never taken for
/// one: it survives a restart and its sweep of leftovers, a deploy, and
/// a branch's removal, and goes with the project.
#[tokio::test]
async fn the_datasets_directory_survives_everything_a_branch_goes_through() {
    let s = server();
    for branch in ["main", "dev"] {
        let a = s.deploy(CS, branch, archive(CS, branch)).await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    }
    let a = s.upload(&format!("/projects/{CS}/datasets?name=budget"), Some("user-ann"), BUDGET).await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    let id = a.body["id"].as_str().unwrap().to_string();
    let dataset = s.datasets_dir("default", CS).join(&id);
    assert!(dataset.join("dataset.json").exists());

    // A restart, which sweeps a deploy's leftovers, loads the branches
    // and nothing else, and leaves the datasets where they are.
    let again = app_over(&s.data_dir);
    let (n, skipped) = reload(&again).unwrap();
    assert_eq!((n, skipped.len()), (2, 0), "{skipped:?}");
    assert!(dataset.join("dataset.json").exists());
    assert_eq!(s.on_disk(), ["default/customer-service/dev", "default/customer-service/main"]);

    let a = s.deploy(CS, "main", archive(CS, "again")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s.send(Method::DELETE, &format!("/projects/{CS}/branches/dev"), &[], None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert!(dataset.join("dataset.json").exists());
    assert_eq!(s.listing().await, ["customer-service/main"]);
    let a = s.as_ann(Method::GET, &format!("/projects/{CS}/datasets/{id}")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);

    // The project's removal takes them.
    let a = s.send(Method::DELETE, &format!("/projects/{CS}"), &[], None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert_eq!(a.body["datasets"], json!([id]));
    assert!(!s.data_dir.join("default").join(CS).exists());
}

#[tokio::test]
async fn a_dataset_through_its_routes() {
    let s = server();
    let a = s.deploy(CS, "main", archive(CS, "main")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let base = format!("/projects/{CS}/datasets");

    // An owner is needed, and a project that is deployed.
    let a = s.upload(&base, None, BUDGET).await;
    assert_eq!((a.status, a.class()), (StatusCode::BAD_REQUEST, "Invalid"), "{}", a.body);
    assert!(a.error().contains("zsql-owner"));
    let a = s.upload("/projects/nowhere/datasets", Some("user-ann"), BUDGET).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND, "{}", a.body);
    let a = s.upload(&format!("{base}?bogus=1"), Some("user-ann"), BUDGET).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.body);

    let a = s.upload(&format!("{base}?file_name=budget.csv"), Some("user-ann"), BUDGET).await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    assert_eq!(a.body["name"], "budget");
    assert!(a.body.get("owner").is_none(), "the owner's hash is not answered");
    assert_eq!(a.body["columns"][2], json!({ "name": "amount", "type": "integer", "role": "measure" }));
    let id = a.body["id"].as_str().unwrap().to_string();

    let a = s.as_ann(Method::GET, &base).await;
    assert_eq!(a.body["datasets"].as_array().unwrap().len(), 1);
    let a = s.send(Method::GET, &base, &[(crate::OWNER_HEADER, "user-bob")], None).await;
    assert_eq!(a.body["datasets"], json!([]));
    let a = s.send(Method::GET, &format!("{base}/{id}"), &[(crate::OWNER_HEADER, "user-bob")], None).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND, "another owner's dataset is not there for them");

    let a = s.as_ann(Method::GET, &format!("{base}/{id}/rows?limit=1")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!((a.body["total"].as_u64(), a.body["version"].as_u64()), (Some(2), Some(1)));
    assert_eq!(a.body["rows"], json!([["EMEA", "2026-01-01", 100]]));
    let a = s.as_ann(Method::GET, &format!("{base}/{id}/rows?limit=0")).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);

    let a = s.as_ann(Method::GET, &format!("{base}/{id}/lineage")).await;
    assert_eq!(a.body["sources"][0]["file_name"], "budget.csv", "{}", a.body);
    let a = s.as_ann(Method::GET, &format!("{base}/{id}/sources/s1")).await;
    assert_eq!(a.body["parse"]["delimiter"], ",", "{}", a.body);

    // A recipe change names the version it was made against.
    let change = json!({ "against": 1, "roles": { "amount": "dimension" } });
    let a = s
        .send(Method::POST, &format!("{base}/{id}/recipe"), &[ann()], Some(("application/json", serde_json::to_vec(&change).unwrap())))
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!((a.body["recipe"].as_u64(), a.body["live"].as_u64()), (Some(2), Some(2)));
    let a = s
        .send(Method::POST, &format!("{base}/{id}/recipe"), &[ann()], Some(("application/json", serde_json::to_vec(&change).unwrap())))
        .await;
    assert_eq!((a.status, a.class()), (StatusCode::CONFLICT, "Conflict"), "{}", a.body);
    let a = s.as_ann(Method::GET, &format!("{base}/{id}/recipe?version=1")).await;
    assert_eq!(a.body["version"], 1);

    // A replacement is a new snapshot; one that will not convert is refused.
    let a = s
        .send(
            Method::PUT,
            &format!("{base}/{id}/upload"),
            &[ann()],
            Some(("text/csv", b"region,month,amount\nEMEA,2026-02-01,1.5\n".to_vec())),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["columns"][2]["type"], "decimal(1)");
    let a = s
        .send(Method::PUT, &format!("{base}/{id}/upload"), &[ann()], Some(("text/csv", b"region,month,amount\nEMEA,later,1\n".to_vec())))
        .await;
    assert_eq!((a.status, a.class()), (StatusCode::UNPROCESSABLE_ENTITY, "Unprocessable"), "{}", a.body);

    // An org's datasets are its own.
    let a = s.deploy_to("acme", CS, "main", archive(CS, "acme")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let a = s.as_ann(Method::GET, &format!("/orgs/acme{base}/{id}")).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);
    let a = s.upload(&format!("/orgs/acme{base}"), Some("user-ann"), BUDGET).await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    assert!(s.datasets_dir("acme", CS).join(a.body["id"].as_str().unwrap()).exists());

    let a = s.as_ann(Method::DELETE, &format!("{base}/{id}")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let a = s.as_ann(Method::GET, &format!("{base}/{id}")).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);

    // An owner's removal takes theirs in the org, and nobody else's.
    let a = s.upload(&base, Some("user-ann"), BUDGET).await;
    let kept = s.upload(&base, Some("user-bob"), BUDGET).await;
    let a2 = s.as_ann(Method::DELETE, "/datasets").await;
    assert_eq!(a2.body["removed"], json!([a.body["id"]]), "{}", a2.body);
    let a = s.send(Method::GET, &format!("{base}/{}", kept.body["id"].as_str().unwrap()), &[(crate::OWNER_HEADER, "user-bob")], None).await;
    assert_eq!(a.status, StatusCode::OK);
    let a = s.as_ann(Method::GET, &format!("/orgs/acme{base}")).await;
    assert_eq!(a.body["datasets"].as_array().unwrap().len(), 1, "another org's are not touched");
}

impl Server {
    async fn post_as_ann(&self, path: &str, body: Value) -> Answer {
        self.send(Method::POST, path, &[ann()], Some(("application/json", serde_json::to_vec(&body).unwrap()))).await
    }
}

/// Steps, a preview, a page sorted and filtered for viewing, what reads
/// a column, and a hold confirmed.
#[tokio::test]
async fn a_recipe_of_steps_through_its_routes() {
    let s = server();
    let a = s.deploy(CS, "main", archive(CS, "main")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let base = format!("/projects/{CS}/datasets");
    let a = s.upload(&base, Some("user-ann"), "region,store,amount\nNY,a,10\nCA,b,5\nNY,c,7\n").await;
    let id = a.body["id"].as_str().unwrap().to_string();
    let steps = json!([
        { "op": "remap", "column": "region", "values": [{ "from": "NY", "to": "New York" }] },
        { "op": "formula", "column": "label", "formula": "region & '/' & store" },
    ]);

    let a = s.post_as_ann(&format!("{base}/{id}/preview"), json!({ "steps": steps, "limit": 1 })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["rows"], json!([["New York", "a", 10, "New York/a"]]));
    let a = s.post_as_ann(&format!("{base}/{id}/preview"), json!({ "steps": [{ "op": "sql", "sql": "select 1" }] })).await;
    assert_eq!(a.status, StatusCode::UNPROCESSABLE_ENTITY, "an unknown step is not a step: {}", a.body);

    let a = s.post_as_ann(&format!("{base}/{id}/recipe"), json!({ "against": 1, "steps": steps })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let a = s.as_ann(Method::GET, &format!("{base}/{id}/rows?sort=amount:desc&filter=amount%20%3C%2010")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["total"], 2);
    assert_eq!(a.body["rows"], json!([["New York", "c", 7, "New York/c"], ["CA", "b", 5, "CA/b"]]));
    let a = s.as_ann(Method::GET, &format!("{base}/{id}/rows?sort=amount:sideways")).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);

    let a = s.as_ann(Method::GET, &format!("{base}/{id}/impact?column=store")).await;
    assert_eq!(a.body["said"], json!(["step 2 (formula) reads column 'store', which is not there at that step"]), "{}", a.body);

    // A new snapshot without `store` holds the dataset; confirming drops the formula.
    let a = s.send(Method::PUT, &format!("{base}/{id}/upload"), &[ann()], Some(("text/csv", b"region,amount\nNY,1\n".to_vec()))).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["held"]["unresolved"][0]["column"], "store", "{}", a.body);
    assert_eq!(a.body["live"], 2);
    let a = s.post_as_ann(&format!("{base}/{id}/confirm"), json!({ "against": 2 })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert!(a.body.get("held").is_none());
    assert_eq!((a.body["live"].as_u64(), a.body["recipe"].as_u64()), (Some(3), Some(3)));
    let a = s.as_ann(Method::GET, &format!("{base}/{id}/rows")).await;
    assert_eq!(a.body["rows"], json!([["New York", 1]]));
}

/// Edits through the route: a remap, an override, and the log.
#[tokio::test]
async fn edits_through_their_routes() {
    let s = server();
    let a = s.deploy(CS, "main", archive(CS, "main")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let base = format!("/projects/{CS}/datasets");
    let id = s.upload(&base, Some("user-ann"), BUDGET).await.body["id"].as_str().unwrap().to_string();
    let a = s
        .post_as_ann(
            &format!("{base}/{id}/edits"),
            json!({ "against": 1, "edits": [
                { "op": "cell", "column": "region", "from": "EMEA", "to": "Europe" },
                { "op": "cell", "column": "amount", "row": { "region": "APAC", "month": "2026-01-01" }, "to": 81 },
            ]}),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["outcomes"].as_array().unwrap().len(), 2);
    assert_eq!(a.body["dataset"]["edits"], 1);
    let a = s.as_ann(Method::GET, &format!("{base}/{id}/rows")).await;
    assert_eq!(a.body["rows"], json!([["Europe", "2026-01-01", 100], ["APAC", "2026-01-01", 81]]));
    let a = s.as_ann(Method::GET, &format!("{base}/{id}/edits")).await;
    assert_eq!(a.body["entries"][0]["edits"][1]["was"], 80, "{}", a.body);
    let a = s
        .post_as_ann(
            &format!("{base}/{id}/edits"),
            json!({ "against": 1, "edits": [{ "op": "cell", "column": "nope", "from": "x", "to": "y" }] }),
        )
        .await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.body);
    assert!(a.error().contains("edit 1: there is no column 'nope'"), "{}", a.body);
}

/// A dataset made from a semantic query, run fresh against the
/// warehouse, and refreshed by request.
#[tokio::test]
async fn a_semantic_query_as_a_source() {
    let s = server();
    let a = s.deploy(CS, "main", archive(CS, "main")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let file = sales("prep");
    let connection = json!({ "file": file.to_str().unwrap() });
    let base = format!("/projects/{CS}/datasets");
    let a = s
        .post_as_ann(
            &format!("{base}/query"),
            json!({ "name": "revenue", "branch": "main", "expr": "category asc, revenue", "connection": connection }),
        )
        .await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    let id = a.body["id"].as_str().unwrap().to_string();
    assert_eq!(a.body["sources"][0]["kind"], "semantic");
    let roles: Vec<(&str, &str)> =
        a.body["columns"].as_array().unwrap().iter().map(|c| (c["name"].as_str().unwrap(), c["role"].as_str().unwrap())).collect();
    assert_eq!(roles[1].1, "measure", "the model says revenue is a measure: {roles:?}");
    assert_eq!(roles[0].1, "dimension");
    let rows = s.as_ann(Method::GET, &format!("{base}/{id}/rows")).await;
    assert_eq!(rows.body["rows"], json!([["books", "30.50"], ["games", "5.25"], ["toys", "1.00"]]), "{}", rows.body);
    let lineage = s.as_ann(Method::GET, &format!("{base}/{id}/lineage")).await;
    let read = &lineage.body["sources"][0]["semantic"];
    assert_eq!((read["branch"].as_str(), read["sql_sha256"].as_str().map(str::len)), (Some("main"), Some(64)), "{}", lineage.body);
    assert!(read["context"].is_string() && read["fields"].is_object(), "{read}");

    let a = s.post_as_ann(&format!("{base}/{id}/refresh"), json!({ "connection": connection })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!((a.body["sources"][0]["snapshot"].as_u64(), a.body["live"].as_u64()), (Some(2), Some(2)));
    let lineage = s.as_ann(Method::GET, &format!("{base}/{id}/lineage")).await;
    assert_eq!(lineage.body["sources"][0]["snapshot"], 2);

    // A refresh is for a query's dataset; a bad query is refused as the planner refuses it.
    let upload = s.upload(&base, Some("user-ann"), BUDGET).await.body["id"].as_str().unwrap().to_string();
    let a = s.post_as_ann(&format!("{base}/{upload}/refresh"), json!({})).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.body);
    let a = s
        .post_as_ann(&format!("{base}/query"), json!({ "branch": "main", "expr": "nothing_by_this_name", "connection": connection }))
        .await;
    assert!(a.status.is_client_error(), "{}", a.body);
    let a = s.post_as_ann(&format!("{base}/query"), json!({ "branch": "nope", "expr": "revenue" })).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND, "{}", a.body);
}

/// A dataset that reads another, and what that means for a delete.
#[tokio::test]
async fn combining_datasets_through_the_routes() {
    let s = server();
    let a = s.deploy(CS, "main", archive(CS, "main")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let base = format!("/projects/{CS}/datasets");
    let budget = s.upload(&base, Some("user-ann"), BUDGET).await.body["id"].as_str().unwrap().to_string();
    let owners = s.upload(&base, Some("user-ann"), "area,manager\nEMEA,kim\nAPAC,lee\n").await.body["id"].as_str().unwrap().to_string();
    let a = s
        .post_as_ann(
            &format!("{base}/{budget}/recipe"),
            json!({ "against": 1, "steps": [{ "op": "join", "dataset": owners, "on": [{ "left": "region", "right": "area" }] }] }),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["columns"][3]["name"], "manager");
    let a = s.as_ann(Method::GET, &format!("{base}/{owners}/downstream")).await;
    assert_eq!(a.body["downstream"][0]["id"], budget.as_str(), "{}", a.body);
    let a = s.as_ann(Method::DELETE, &format!("{base}/{owners}")).await;
    assert_eq!(a.status, StatusCode::CONFLICT, "{}", a.body);
    let a = s.as_ann(Method::DELETE, &format!("{base}/{owners}?confirm=true")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
}

#[tokio::test]
async fn datasets_can_be_turned_off() {
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
        default_sql_limit: None,
        prep: None,
    });
    let s = Server { router: router(app.clone()), app, data_dir };
    let a = s.as_ann(Method::GET, &format!("/projects/{CS}/datasets")).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);
    assert!(a.error().contains("ZSQL_DATASETS=off"), "{}", a.body);
}

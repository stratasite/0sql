//! Datasets through the router: the routes, and the
//! `.datasets` directory beside a project's branches.

use super::*;

const BUDGET: &str = "region,month,amount\nEMEA,2026-01-01,100\nAPAC,2026-01-01,80\n";

impl Server {
    async fn upload(&self, path: &str, csv: &str) -> Answer {
        self.send(Method::POST, path, &[], Some(("text/csv", csv.as_bytes().to_vec()))).await
    }

    async fn call(&self, method: Method, path: &str) -> Answer {
        self.send(method, path, &[], None).await
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
    let (src, id) = s.upload_dataset(BUDGET).await;
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
    let a = s.call(Method::GET, &format!("/projects/{CS}/datasets/{id}")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);

    // The project's removal takes them.
    let a = s.send(Method::DELETE, &format!("/projects/{CS}"), &[], None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert_eq!(a.body["datasets"], json!([id, src]));
    assert!(!s.data_dir.join("default").join(CS).exists());
}

impl Server {
    async fn post_json(&self, path: &str, body: Value) -> Answer {
        self.send(Method::POST, path, &[], Some(("application/json", serde_json::to_vec(&body).unwrap()))).await
    }

    /// An upload source and a dataset over it: their ids.
    async fn upload_dataset(&self, csv: &str) -> (String, String) {
        let a = self.upload(&format!("/projects/{CS}/sources?dataset=true&file_name=budget.csv"), csv).await;
        assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
        (a.body["source"]["id"].as_str().unwrap().to_string(), a.body["dataset"]["id"].as_str().unwrap().to_string())
    }
}

fn steps_graph(source: &str, steps: Value) -> Value {
    json!({ "nodes": [
        { "id": "in", "kind": "source", "source": source },
        { "id": "shape", "kind": "steps", "input": "in", "steps": steps },
        { "id": "out", "kind": "output", "input": "shape" },
    ]})
}

#[tokio::test]
async fn sources_and_datasets_through_their_routes() {
    let s = server();
    let a = s.deploy(CS, "main", archive(CS, "main")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let sources = format!("/projects/{CS}/sources");
    let datasets = format!("/projects/{CS}/datasets");

    // A source needs a project that is deployed.
    let a = s.upload("/projects/nowhere/sources", BUDGET).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND, "{}", a.body);
    let a = s.upload(&format!("{sources}?bogus=1"), BUDGET).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.body);
    let a = s.upload(&format!("{sources}?kind=nope"), BUDGET).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.body);

    // An upload used as it is: a source, and a dataset over it that copies nothing.
    let a = s.upload(&format!("{sources}?file_name=budget.csv&dataset=true&description=the%20plan"), BUDGET).await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    assert_eq!((a.body["source"]["name"].as_str(), a.body["source"]["kind"].as_str()), (Some("budget"), Some("upload")));
    assert_eq!(a.body["snapshot"]["parse"]["delimiter"], ",", "{}", a.body);
    assert_eq!(a.body["dataset"]["columns"][2], json!({ "name": "amount", "type": "integer", "role": "measure" }));
    let (src, id) = (a.body["source"]["id"].as_str().unwrap().to_string(), a.body["dataset"]["id"].as_str().unwrap().to_string());
    assert_eq!(s.call(Method::GET, &sources).await.body["sources"].as_array().unwrap().len(), 1);
    assert_eq!(s.call(Method::GET, &datasets).await.body["datasets"].as_array().unwrap().len(), 1);
    let a = s.call(Method::GET, &format!("{datasets}/{id}/rows?limit=1")).await;
    assert_eq!((a.body["total"].as_u64(), a.body["version"].as_u64()), (Some(2), Some(1)), "{}", a.body);
    assert_eq!(a.body["rows"], json!([["EMEA", "2026-01-01", 100]]));
    assert_eq!(s.call(Method::GET, &format!("{datasets}/{id}/rows?limit=0")).await.status, StatusCode::BAD_REQUEST);
    let a = s.call(Method::GET, &format!("{datasets}/{id}/versions/1")).await;
    assert_eq!(a.body["manifest"]["pointer"]["source"], src.as_str(), "{}", a.body);
    assert_eq!(a.body["graph"]["nodes"][0]["kind"], "source");
    let a = s.call(Method::GET, &format!("{datasets}/{id}/lineage")).await;
    assert_eq!(a.body["sources"][0]["file_name"], "budget.csv", "{}", a.body);

    // A new file is a new snapshot, and runs nothing.
    let a = s.upload(&format!("{sources}/{src}/snapshots"), "region,month,amount\nEMEA,2026-02-01,1.5\n").await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    assert_eq!(a.body["snapshot"]["number"], 2);
    assert_eq!(s.call(Method::GET, &format!("{datasets}/{id}")).await.body["live"], 1);
    let a = s.upload(&format!("{sources}/{src}/snapshots"), "region,month,amount\nEMEA,later,1\n").await;
    assert_eq!((a.status, a.class()), (StatusCode::UNPROCESSABLE_ENTITY, "Unprocessable"), "{}", a.body);
    let a = s.call(Method::GET, &format!("{sources}/{src}/snapshots")).await;
    assert_eq!(a.body["snapshots"].as_array().unwrap().len(), 2);
    let a = s.call(Method::GET, &format!("{sources}/{src}/rows?snapshot=1&sort=amount:desc")).await;
    assert_eq!(a.body["rows"][0], json!(["EMEA", "2026-01-01", 100]), "{}", a.body);
    assert_eq!(s.call(Method::GET, &format!("{sources}/{src}/rows?sort=amount:sideways")).await.status, StatusCode::BAD_REQUEST);
    let a = s
        .send(
            Method::PATCH,
            &format!("{sources}/{src}"),
            &[],
            Some(("application/json", br#"{"name": "plan", "types": {"amount": "text"}}"#.to_vec())),
        )
        .await;
    assert_eq!((a.status, a.body["name"].as_str(), a.body["read"].as_u64()), (StatusCode::OK, Some("plan"), Some(2)), "{}", a.body);

    // Pushed rows, and a dataset made empty for a run to fill.
    let a =
        s.post_json(&format!("{sources}?kind=push&name=deals"), json!({ "rows": [{ "deal": "Acme", "value": 3 }], "origin": "crm" })).await;
    assert_eq!((a.status, a.body["snapshot"]["origin"].as_str()), (StatusCode::CREATED, Some("crm")), "{}", a.body);
    let a = s.post_json(&datasets, json!({ "name": "empty" })).await;
    assert_eq!((a.status, a.body["live"].clone()), (StatusCode::CREATED, Value::Null), "{}", a.body);

    // An org's are its own.
    let a = s.deploy_to("acme", CS, "main", archive(CS, "acme")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    assert_eq!(s.call(Method::GET, &format!("/orgs/acme{datasets}/{id}")).await.status, StatusCode::NOT_FOUND);
    assert_eq!(s.call(Method::GET, &format!("/orgs/acme{sources}/{src}")).await.status, StatusCode::NOT_FOUND);

    let a = s.call(Method::DELETE, &format!("{sources}/{src}")).await;
    assert_eq!(a.body["datasets_given_their_rows"], json!([id]), "{}", a.body);
    assert_eq!(s.call(Method::GET, &format!("{datasets}/{id}/rows")).await.body["total"], 2, "it still answers");
    assert_eq!(s.call(Method::DELETE, &format!("{datasets}/{id}")).await.status, StatusCode::OK);
    assert_eq!(s.call(Method::GET, &format!("{datasets}/{id}")).await.status, StatusCode::NOT_FOUND);
}

/// A run of a graph: a dry run first, a preview of a node, the run, its
/// record, and a retry answered with it.
#[tokio::test]
async fn a_run_of_a_graph_through_its_routes() {
    let s = server();
    let a = s.deploy(CS, "main", archive(CS, "main")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let (src, id) = s.upload_dataset("region,store,amount\nNY,a,10\nCA,b,5\nNY,c,7\n").await;
    let steps = json!([
        { "op": "remap_val", "column": "region", "values": [{ "from": "NY", "to": "New York" }] },
        { "op": "add_column", "column": "label", "formula": "region & '/' & store" },
    ]);
    let graph = steps_graph(&src, steps);
    let runs = format!("/projects/{CS}/runs");

    let a = s.post_json(&format!("{runs}?check=true"), json!({ "graph": graph })).await;
    assert_eq!((a.status, a.body["ok"].as_bool()), (StatusCode::OK, Some(true)), "{}", a.body);
    assert_eq!(a.body["nodes"][1]["columns"][3]["name"], "label");
    assert_eq!(a.body["reads"], json!([{ "kind": "source", "id": src }]));

    let a = s.post_json(&format!("/projects/{CS}/preview"), json!({ "graph": graph, "node": "shape", "limit": 1 })).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["rows"], json!([["New York", "a", 10, "New York/a"]]));
    assert_eq!(a.body["nodes"][1]["steps"][0]["said"], "remapped 2 value(s) of region");

    let a = s.post_json(&runs, json!({ "id": "r-1", "dataset": id, "graph": graph })).await;
    assert_eq!(
        (a.status, a.body["state"].as_str(), a.body["version"].as_u64()),
        (StatusCode::OK, Some("succeeded"), Some(2)),
        "{}",
        a.body
    );
    assert!(a.body["nodes"][1]["summary"].as_str().unwrap().starts_with("2 steps: 3 rows in, 3 out"), "{}", a.body);
    let a = s.call(Method::GET, &format!("/projects/{CS}/datasets/{id}/rows?sort=amount:desc&filter=amount%20%3C%2010")).await;
    assert_eq!(a.body["rows"], json!([["New York", "c", 7, "New York/c"], ["CA", "b", 5, "CA/b"]]), "{}", a.body);
    let a = s.call(Method::GET, &format!("{runs}/r-1")).await;
    assert_eq!((a.status, a.body["version"].as_u64()), (StatusCode::OK, Some(2)));
    let a = s.post_json(&runs, json!({ "id": "r-1", "dataset": id, "graph": graph })).await;
    assert_eq!(a.body["version"], 2, "a retry is answered, not run again: {}", a.body);
    let a = s.post_json(&runs, json!({ "id": "r-1", "dataset": id, "graph": steps_graph(&src, json!([])) })).await;
    assert_eq!(a.status, StatusCode::CONFLICT, "{}", a.body);

    // Refused before it runs: each problem, by node.
    let bad = steps_graph(&src, json!([{ "op": "trim_val", "columns": ["regoin"] }]));
    let a = s.post_json(&runs, json!({ "id": "r-2", "dataset": id, "graph": bad })).await;
    assert_eq!((a.status, a.class()), (StatusCode::UNPROCESSABLE_ENTITY, "Refused"), "{}", a.body);
    assert_eq!(a.body["problems"][0]["node"], "shape");
    assert_eq!(a.body["problems"][0]["code"], "missing_column");
    assert_eq!(a.body["problems"][0]["suggestions"][0]["column"], "region");
    let a = s.post_json(&runs, json!({ "dataset": id, "graph": graph })).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "a run says its id: {}", a.body);
    let a =
        s.post_json(&runs, json!({ "id": "r-3", "dataset": id, "graph": { "nodes": [{ "id": "x", "kind": "sql", "sql": "1" }] } })).await;
    assert_eq!(a.status, StatusCode::UNPROCESSABLE_ENTITY, "an unknown field is no graph: {}", a.body);
    assert_eq!(s.call(Method::GET, &format!("{runs}/nope")).await.status, StatusCode::NOT_FOUND);

    // Edits ride in the graph, by key.
    let edited = json!({ "nodes": [
        { "id": "in", "kind": "source", "source": src },
        { "id": "fix", "kind": "edits", "input": "in", "key": ["store"], "edits": [
            { "id": "e1", "kind": "set", "column": "amount", "key": { "store": "b" }, "to": 6, "was": 5 },
            { "id": "e2", "kind": "remap", "column": "region", "from": "CA", "to": "California" },
        ]},
        { "id": "out", "kind": "output", "input": "fix", "key": ["store"] },
    ]});
    let a = s.post_json(&runs, json!({ "id": "r-4", "dataset": id, "graph": edited })).await;
    assert_eq!(a.body["state"], "succeeded", "{}", a.body);
    assert_eq!(a.body["nodes"][1]["edits"].as_array().unwrap().len(), 2);
    let a = s.call(Method::GET, &format!("/projects/{CS}/datasets/{id}/rows")).await;
    assert_eq!(a.body["rows"][1], json!(["California", "b", 6]), "{}", a.body);
}

/// A query source: pulled by a run, as the run's context, from the
/// warehouse; its snapshot read again only as that context.
#[tokio::test]
async fn a_query_source_is_pulled_by_a_run() {
    let s = server();
    let a = s.deploy(CS, "main", archive(CS, "main")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let file = sales("prep");
    let connection = json!({ "file": file.to_str().unwrap() });
    let a = s
        .post_json(&format!("/projects/{CS}/sources?kind=query&name=revenue"), json!({ "branch": "main", "expr": "category asc, revenue" }))
        .await;
    assert_eq!((a.status, a.body["source"]["snapshot"].as_u64()), (StatusCode::CREATED, Some(0)), "{}", a.body);
    let src = a.body["source"]["id"].as_str().unwrap().to_string();
    let id = s.post_json(&format!("/projects/{CS}/datasets"), json!({ "name": "revenue" })).await.body["id"].as_str().unwrap().to_string();
    let graph = json!({ "nodes": [
        { "id": "rev", "kind": "source", "source": src },
        { "id": "out", "kind": "output", "input": "rev" },
    ]});
    let a = s.post_json(&format!("/projects/{CS}/runs?check=true"), json!({ "graph": graph })).await;
    assert!(a.body["said"].as_str().unwrap().starts_with("Pulls revenue (it has never been pulled)"), "{}", a.body);
    let a =
        s.post_json(&format!("/projects/{CS}/runs"), json!({ "id": "q-1", "dataset": id, "graph": graph, "connection": connection })).await;
    assert_eq!(a.body["state"], "succeeded", "{}", a.body);
    assert_eq!(a.body["inputs"][0]["will"], "pull");
    let a = s.call(Method::GET, &format!("/projects/{CS}/datasets/{id}")).await;
    let roles: Vec<&str> = a.body["columns"].as_array().unwrap().iter().map(|c| c["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["dimension", "measure"], "the model says revenue is a measure");
    let rows = s.call(Method::GET, &format!("/projects/{CS}/datasets/{id}/rows")).await;
    assert_eq!(rows.body["rows"], json!([["books", "30.50"], ["games", "5.25"], ["toys", "1.00"]]), "{}", rows.body);
    let lineage = s.call(Method::GET, &format!("/projects/{CS}/datasets/{id}/lineage")).await;
    let read = &lineage.body["sources"][0]["semantic"];
    assert_eq!((read["branch"].as_str(), read["sql_sha256"].as_str().map(str::len)), (Some("main"), Some(64)), "{}", lineage.body);
    // The same context reads the snapshot it took; a preview does not pull.
    let a =
        s.post_json(&format!("/projects/{CS}/runs"), json!({ "id": "q-2", "dataset": id, "graph": graph, "connection": connection })).await;
    assert_eq!(a.body["inputs"][0]["will"], "read", "{}", a.body);
    let a = s.post_json(&format!("/projects/{CS}/preview"), json!({ "graph": graph, "connection": connection })).await;
    assert_eq!(a.body["rows"].as_array().unwrap().len(), 3, "{}", a.body);
    // A query the planner refuses is refused before anything is pulled,
    // by node, and nothing is recorded.
    let a = s.post_json(&format!("/projects/{CS}/sources?kind=query"), json!({ "branch": "main", "expr": "nothing_by_this_name" })).await;
    let bad = a.body["source"]["id"].as_str().unwrap().to_string();
    let g = json!({ "nodes": [{ "id": "x", "kind": "source", "source": bad }, { "id": "out", "kind": "output", "input": "x" }] });
    let a = s.post_json(&format!("/projects/{CS}/runs"), json!({ "id": "q-3", "dataset": id, "graph": g, "connection": connection })).await;
    assert_eq!((a.status, a.class()), (StatusCode::UNPROCESSABLE_ENTITY, "Refused"), "{}", a.body);
    assert_eq!((a.body["problems"][0]["node"].as_str(), a.body["problems"][0]["code"].as_str()), (Some("x"), Some("plan")), "{}", a.body);
    assert_eq!(s.call(Method::GET, &format!("/projects/{CS}/runs/q-3")).await.status, StatusCode::NOT_FOUND);
    let a = s.post_json(&format!("/projects/{CS}/runs?check=true"), json!({ "graph": g })).await;
    assert_eq!(a.body["ok"], false, "{}", a.body);
    // The preview kept nothing: the one snapshot is the run's.
    let a = s.call(Method::GET, &format!("/projects/{CS}/sources/{src}/snapshots")).await;
    assert_eq!(a.body["snapshots"].as_array().unwrap().len(), 1, "{}", a.body);
    let a = s.post_json(&format!("/projects/{CS}/sources?kind=query"), json!({ "branch": "../x", "expr": "revenue" })).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.body);
}

/// Budget against actual: the model's part run in the warehouse, the
/// dataset beside it in DuckDB, rows from both sides kept.
#[tokio::test]
async fn a_blend_sets_a_dataset_beside_the_model() {
    let s = server();
    let a = s.deploy(CS, "main", archive(CS, "main")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let file = sales("blend");
    let connection = json!({ "file": file.to_str().unwrap() });
    let budget = s.upload_dataset("category,amount\nbooks,40\ngames,5\nmusic,7\n").await.1;
    let targets = s.upload_dataset("category\nbooks\n").await.1;
    let route = format!("/projects/{CS}/branches/main/blend");
    let ask = |datasets: Value, blend: Value, projections: Value| json!({ "spec": { "projections": projections }, "datasets": datasets, "blend": blend, "connection": connection });
    let enrich = ask(
        json!([{ "id": budget, "as": "budget" }]),
        json!([{ "join": "budget", "to": "model", "on": [{ "left": "Category", "right": "budget.category" }], "kind": "enrich" }]),
        json!([{ "field": "Category" }, { "field": "Revenue" }, { "field": "budget.amount" }]),
    );
    let a = s.post_json(&route, enrich.clone()).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let mut rows = a.body["rows"].as_array().unwrap().clone();
    rows.sort_by_key(|r| r.to_string());
    assert_eq!(
        rows,
        [json!(["books", "30.50", 40]), json!(["games", "5.25", 5]), json!(["music", null, 7]), json!(["toys", "1.00", null]),],
        "a category with a budget and no sales still shows"
    );
    assert_eq!(a.body["columns"][2]["name"], "budget.amount");
    assert_eq!(a.body["cache"], "miss");
    assert_eq!(a.body["datasets"][0]["id"], budget.as_str());
    assert!(a.body["model"]["sql"].as_str().unwrap().contains("sales"));
    let again = s.post_json(&route, enrich.clone()).await;
    assert_eq!(again.body["cache"], "hit", "{}", again.body);

    let filter = ask(
        json!([{ "id": targets, "as": "targets" }]),
        json!([{ "join": "targets", "to": "model", "on": [{ "left": "Category", "right": "targets.category" }], "kind": "filter" }]),
        json!([{ "field": "Category" }, { "field": "Revenue" }]),
    );
    let a = s.post_json(&route, filter).await;
    assert_eq!(a.body["rows"], json!([["books", "30.50"]]), "{}", a.body);

    // A join key the request does not ask for is refused, before the warehouse is asked.
    let hidden = ask(
        json!([{ "id": budget, "as": "budget" }]),
        json!([{ "join": "budget", "to": "model", "on": [{ "left": "Category", "right": "budget.category" }], "kind": "enrich" }]),
        json!([{ "field": "Revenue" }, { "field": "budget.amount" }]),
    );
    let a = s.post_json(&route, hidden).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.body);
    assert!(a.error().contains("'Category' joins 'budget' to the model, so ask for it too"), "{}", a.body);

    // Attached and joined to nothing, an alias the model has, another owner's.
    let lonely = ask(json!([{ "id": budget, "as": "budget" }]), json!([]), json!([{ "field": "Category" }]));
    let a = s.post_json(&route, lonely).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.body);
    assert!(a.error().contains("joined to nothing"), "{}", a.body);
    let clash = ask(
        json!([{ "id": budget, "as": "sales" }]),
        json!([{ "join": "sales", "to": "model", "on": [{ "left": "Category", "right": "sales.category" }], "kind": "enrich" }]),
        json!([{ "field": "Category" }]),
    );
    let a = s.post_json(&route, clash).await;
    assert!(a.error().contains("is a name in the model"), "{}", a.body);
    let mut missing = enrich.clone();
    missing["datasets"][0]["id"] = json!("ds_0000000000000000");
    let a = s.post_json(&route, missing).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND, "{}", a.body);
}

/// Datasets are in no catalog and change no plan: a model plans byte
/// for byte the same with datasets present and none attached.
#[tokio::test]
async fn datasets_change_nothing_the_model_plans() {
    let s = server();
    let a = s.deploy(CS, "main", archive(CS, "main")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.error());
    let (sql, fields, tables_path) = (
        format!("/projects/{CS}/branches/main/sql"),
        format!("/projects/{CS}/branches/main/fields?hidden=true"),
        format!("/projects/{CS}/branches/main/tables"),
    );
    let plan = || s.post(&sql, json!({ "expr": "category asc, revenue" }));
    let catalog = || s.get(&fields);
    let tables = || s.get(&tables_path);
    let before = (plan().await.body, catalog().await.body, tables().await.body);
    for csv in ["category,amount\nbooks,1\n", "Revenue,Category\n1,x\n"] {
        s.upload_dataset(csv).await;
    }
    let after = (plan().await.body, catalog().await.body, tables().await.body);
    assert_eq!(before.0["sql"], after.0["sql"], "the same SQL, byte for byte");
    assert_eq!(before, after);
    // An MCP client is told of no dataset either.
    let listed = s.mcp(&[], json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })).await;
    assert!(!listed.body.to_string().contains("dataset"), "{}", listed.body);
}

/// A restore needs an object store to restore from.
#[tokio::test]
async fn a_restore_without_an_object_store_is_refused() {
    let s = server();
    let a = s.post("/datasets/restore", json!({})).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.body);
    assert!(a.error().contains("ZSQL_OBJECT_STORE"), "{}", a.body);
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
    let a = s.call(Method::GET, &format!("/projects/{CS}/datasets")).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);
    assert!(a.error().contains("ZSQL_DATASETS=off"), "{}", a.body);
}

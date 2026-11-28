use crate::{unlimited, Kind, Model, SqlRequest, ValueCatalog};
use serde_json::{json, Value};
use zsql_dialect::Dialects;
use zsql_golden::{Fixtures, PathSource};

/// A model is shared across threads by the server; nothing added to
/// it may take that away.
#[test]
fn a_model_is_send_and_sync() {
    fn assert<T: Send + Sync>() {}
    assert::<Model>();
}

fn dialects() -> &'static Dialects {
    Box::leak(Box::new(Dialects::builtin()))
}

/// Every golden case planned over its universe's stored paths, as a
/// model does: the SQL for each, or the error, in case order.
fn plan_all(fixtures: &Fixtures, dialects: &'static Dialects) -> Vec<String> {
    let mut out = Vec::new();
    let mut models: std::collections::HashMap<String, Model> = Default::default();
    for cases in fixtures.cases().expect("golden cases").into_values() {
        for case in cases {
            let model = models.entry(case.universe.clone()).or_insert_with(|| {
                Model::build("g", &case.universe, fixtures.snapshot(&case.universe).expect("snapshot"), dialects).expect("universe")
            });
            let req = SqlRequest { spec: Some(case.spec.clone()), expr: None, context: case.context.clone() };
            out.push(match model.sql(req, &unlimited) {
                Ok(r) => format!("{}: {}", case.id, r.sql),
                Err(e) => format!("{}: ERROR {e}", case.id),
            });
        }
    }
    out
}

/// The planner numbers projections from a thread-local counter.
/// Whatever thread plans, and however many plans came before it on
/// that thread, the SQL must come out the same; otherwise the
/// server, which plans on a pool, and the CLI, which plans on its
/// main thread, would disagree.
#[test]
fn sql_is_the_same_on_every_thread() {
    let fx = Fixtures::open(Fixtures::default_root());
    let d = dialects();
    let here = plan_all(&fx, d);
    let there = std::thread::spawn(move || {
        // Advance the counter first, so the ids this thread hands
        // out do not line up with the main thread's by accident.
        let fx = Fixtures::open(Fixtures::default_root());
        let _ = plan_all(&fx, d);
        plan_all(&fx, d)
    })
    .join()
    .expect("planning thread");
    assert!(!here.is_empty(), "no golden cases found");
    assert_eq!(here, there);
    let _ = PathSource::Formed;
}

/// The same, across processes: a child of this test binary plans
/// everything and reports a digest, which must match this process's.
#[test]
fn sql_is_the_same_in_another_process() {
    const CHILD: &str = "ZSQL_SERVICE_DETERMINISM_CHILD";
    let fx = Fixtures::open(Fixtures::default_root());
    let here = digest(&plan_all(&fx, dialects()));
    if std::env::var_os(CHILD).is_some() {
        println!("DIGEST {here}");
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["tests::sql_is_the_same_in_another_process", "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .expect("child test process");
    let text = String::from_utf8_lossy(&out.stdout);
    // With --nocapture the harness has already printed `test name ...` on the line.
    let there = text
        .lines()
        .find_map(|l| l.find("DIGEST ").map(|i| l[i + 7..].trim()))
        .unwrap_or_else(|| panic!("no digest from the child:\n{text}\n{}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(here, there);
}

fn digest(lines: &[String]) -> String {
    // FNV-1a over the whole text: no dependency, and a mismatch is
    // all the test needs to detect.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in lines.join("\n").bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// A model written to disk and read back answers the same.
#[test]
fn a_stored_model_comes_back_whole() {
    let fx = Fixtures::open(Fixtures::default_root());
    let d = dialects();
    let hash = fx.snapshot_hashes().expect("hashes").into_iter().next().expect("a universe");
    let model = Model::build("p", "b", fx.snapshot(&hash).expect("snapshot"), d).expect("universe");
    let dir = std::env::temp_dir().join(format!("zsql-core-stored-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    model.write(&dir).expect("write");
    let back = Model::from_stored(&dir, "p", "b", d).expect("read back");
    assert_eq!(back.summary()["fields"], model.summary()["fields"]);
    assert_eq!(back.tables(""), model.tables(""));
    assert_eq!(back.fields("", true), model.fields("", true));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Each list keeps what holds its `q`, in any case, and is whole
/// without one.
#[test]
fn a_list_keeps_what_holds_its_q() {
    use zsql_project::values::{DimensionValues, ValueCount};
    let fx = Fixtures::open(Fixtures::default_root());
    let hash = fx.snapshot_hashes().expect("hashes").into_iter().next().expect("a universe");
    let mut model = Model::build("p", "b", fx.snapshot(&hash).expect("snapshot"), dialects()).expect("universe");
    let names = |v: &Value, list: &str, key: &str| -> Vec<String> {
        v[list].as_array().expect("a list").iter().map(|x| x[key].as_str().unwrap_or_default().to_string()).collect()
    };

    let all = names(&model.tables(""), "tables", "name");
    assert_eq!(names(&model.tables("   "), "tables", "name"), all, "a blank q filters nothing");
    let asked = format!("  {}  ", all[0].to_uppercase());
    let hit = model.tables(&asked);
    assert!(names(&hit, "tables", "name").contains(&all[0]), "found in any case, untrimmed");
    for t in hit["tables"].as_array().unwrap() {
        let holds = |k: &str| t[k].as_str().unwrap_or_default().to_lowercase().contains(&all[0].to_lowercase());
        assert!(holds("uid") || holds("name") || holds("physical_name"), "{t}");
    }
    assert!(names(&model.tables("no table is called this"), "tables", "name").is_empty());

    let sources = model.datasources("");
    let first = &sources["datasources"][0];
    let (uid, adapter) = (first["uid"].as_str().unwrap(), first["adapter"].as_str().unwrap());
    assert_eq!(model.datasource(&uid.to_uppercase()).map(|d| d.adapter.as_str()), Some(adapter));
    assert!(model.datasource("no-such-datasource").is_none());
    assert!(names(&model.datasources(adapter), "datasources", "uid").contains(&uid.to_string()));
    assert!(names(&model.datasources("no datasource is called this"), "datasources", "uid").is_empty());

    let dim = model.semantic().fields().iter().find(|f| f.is_dimension()).expect("a dimension").uid.clone();
    let count = |v: &str, n: i64| ValueCount { v: v.into(), n };
    model.values = Some(ValueCatalog {
        dimensions: vec![DimensionValues {
            uid: dim.clone(),
            name: "d".into(),
            table: "t".into(),
            sampled_at: "2026-01-01T00:00:00Z".into(),
            sample_size: None,
            distinct: Some(3),
            values: vec![count("Northeast", 5), count("North", 3), count("South", 1)],
            range: None,
        }],
    });
    assert_eq!(names(&model.values_of(&dim, "").unwrap(), "values", "v"), ["Northeast", "North", "South"]);
    let north = model.values_of(&dim, "NORTH").unwrap();
    assert_eq!(names(&north, "values", "v"), ["Northeast", "North"], "most frequent first, still");
    assert_eq!(north["distinct"], 3, "the counts describe the sample, not the filter");
    assert_eq!(model.values_of(&dim, "nowhere").unwrap()["values"], json!([]), "no match is an empty list, not a refusal");
}

/// A refusal names its class and kind the way the API always has.
#[test]
fn refusals_carry_their_class() {
    let fx = Fixtures::open(Fixtures::default_root());
    let d = dialects();
    // A universe with policies would refuse everything below for
    // the want of a context, which is its own test.
    let (open, guarded) = fx
        .snapshot_hashes()
        .expect("hashes")
        .into_iter()
        .map(|h| fx.snapshot(&h).expect("snapshot"))
        .fold((None, None), |(o, g), s| if s.security_policies.is_empty() { (o.or(Some(s)), g) } else { (o, g.or(Some(s))) });
    let model = Model::build("p", "b", open.expect("a universe without policies"), d).expect("universe");
    let e = model.sql(SqlRequest::default(), &unlimited).unwrap_err();
    assert_eq!((e.kind, e.class), (Kind::Invalid, "Invalid"));
    let secured = Model::build("p", "b", guarded.expect("a universe with policies"), d).expect("universe");
    let e = secured.sql(SqlRequest { spec: None, expr: Some("anything".into()), context: None }, &unlimited).unwrap_err();
    assert_eq!((e.kind, e.class), (Kind::Invalid, "ContextRequired"));
    let e = model.values_of("no-such-dimension", "").unwrap_err();
    assert_eq!(e.kind, Kind::NotFound);
    let e = model.sql(SqlRequest { spec: None, expr: Some("no such field at all".into()), context: None }, &unlimited).unwrap_err();
    assert_eq!(e.kind, Kind::Unprocessable, "{e}");
}

/// A project whose fields say how they are shown: a format, a display
/// type, both, or neither.
fn presented(root: &std::path::Path, fields: &str) -> Result<Model, crate::LoadError> {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root.join("models")).unwrap();
    std::fs::write(root.join("project.yml"), "name: P\nuid: p\n").unwrap();
    std::fs::write(root.join("datasources.yml"), "wh:\n  adapter: postgres\n").unwrap();
    std::fs::write(
        root.join("models/tbl.accounts.yml"),
        format!("name: Accounts\nphysical_name: accounts\ndatasource: wh\ncost: 1\nfields:\n{fields}"),
    )
    .unwrap();
    Model::from_dir(root, crate::Trust::Owned, dialects())
}

const PRESENTED: &str = "  - type: dimension
    name: Account Id
    data_type: integer
    hidden: true
    expression: id
  - type: dimension
    name: Account Name
    data_type: string
    display_type: { type: link, href: 'https://crm.example.com/{{value}}' }
    expression: name
  - type: dimension
    name: Signed Up
    data_type: date
    format: date:short
    expression: signed_up
  - type: measure
    name: Revenue
    data_type: decimal
    format: currency:2
    expression: sum(revenue)
  - type: measure
    name: Accounts
    data_type: integer
    expression: count(*)
";

fn columns_of(model: &Model, spec: Value) -> Vec<Value> {
    let r = model.sql(SqlRequest { spec: Some(spec), expr: None, context: None }, &unlimited).expect("plans");
    serde_json::to_value(&r).unwrap()["columns"].as_array().cloned().unwrap()
}

#[test]
fn columns_carry_the_fields_format_and_display_type() {
    let root = std::env::temp_dir().join(format!("zsql-presented-{}", std::process::id()));
    let model = presented(&root, PRESENTED).expect("loads");
    let cols = columns_of(&model, json!({ "projections": [{ "field": "Account Name" }, { "field": "Revenue" }, { "field": "Accounts" }] }));
    assert_eq!(cols.len(), 3);
    assert_eq!(cols[0]["name"], "Account Name");
    assert_eq!(cols[0]["kind"], "dimension");
    assert_eq!(cols[0]["data_type"], "string");
    assert_eq!(cols[0]["display_type"], json!({ "type": "link", "href": "https://crm.example.com/{{value}}" }));
    assert_eq!(cols[1]["format"], json!({ "type": "currency", "precision": 2 }));
    assert!(cols[2].get("format").is_none() && cols[2].get("display_type").is_none(), "{}", cols[2]);

    // fields() says the same of the field.
    let f = model.fields("Revenue", false);
    assert_eq!(f["fields"][0]["format"], json!({ "type": "currency", "precision": 2 }));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_projection_overrides_and_reads_other_columns() {
    let root = std::env::temp_dir().join(format!("zsql-presented-over-{}", std::process::id()));
    let model = presented(&root, PRESENTED).expect("loads");
    let cols = columns_of(
        &model,
        json!({ "projections": [
            { "field": "Account Id", "hidden": true },
            { "field": "Account Name", "display_type": { "type": "link", "href": "https://crm.example.com/accounts/{{account id}}" } },
            { "field": "Revenue", "format": "raw" },
            { "field": "Accounts", "format": "number:0", "display_type": "default" },
        ] }),
    );
    assert_eq!(cols[0]["hidden"], true);
    // The slot is spelled as the column it reads.
    assert_eq!(cols[1]["display_type"]["href"], "https://crm.example.com/accounts/{{Account Id}}");
    assert!(cols[2].get("format").is_none(), "raw clears the field's: {}", cols[2]);
    assert_eq!(cols[3]["format"], json!({ "type": "number", "precision": 0 }));

    // `none` clears a field's display type.
    let cols = columns_of(&model, json!({ "projections": [{ "field": "Account Name", "display_type": "none" }] }));
    assert!(cols[0].get("display_type").is_none(), "{}", cols[0]);

    // A slot naming a column the query does not carry is refused.
    let e = model
        .sql(
            SqlRequest {
                spec: Some(json!({ "projections": [{ "field": "Account Name", "display_type": { "type": "link", "href": "/a/{{Account Id}}" } }] })),
                expr: None,
                context: None,
            },
            &unlimited,
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("not a column of this query"), "{e}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_decorator_that_changes_the_value_changes_how_it_shows() {
    let root = std::env::temp_dir().join(format!("zsql-presented-dec-{}", std::process::id()));
    let model = presented(&root, PRESENTED).expect("loads");
    let cols = columns_of(
        &model,
        json!({ "projections": [
            { "field": "Signed Up", "decorators": [{ "type": "truncate", "grain": "month" }] },
            { "field": "Signed Up", "alias": "Weekday", "decorators": [{ "type": "extract", "extract": "day_name" }] },
            { "field": "Revenue" },
            { "field": "Revenue", "decorators": [{ "type": "contribute" }] },
        ] }),
    );
    // A month keeps the date's format and says its grain.
    assert_eq!(cols[0]["grain"], "month");
    assert_eq!(cols[0]["format"]["type"], "date");
    // A weekday is not a date.
    assert_eq!(cols[1]["extract"], "day_name");
    assert!(cols[1].get("format").is_none() && cols[1].get("data_type").is_none(), "{}", cols[1]);
    // A share of the total is a percent, not money.
    assert_eq!(cols[3]["format"], json!({ "type": "percent" }));
    assert_eq!((cols[3]["field_uid"].as_str(), cols[3].get("hidden")), (Some("revenue"), None), "{}", cols[3]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_loader_refuses_what_a_client_could_not_show() {
    let root = std::env::temp_dir().join(format!("zsql-presented-bad-{}", std::process::id()));
    let field = |extra: &str| format!("  - type: dimension\n    name: Name\n    data_type: string\n{extra}    expression: name\n");
    for (extra, says) in [
        ("    format: nope\n", "not a format"),
        ("    format: 'html:<b>{{value}}</b>'\n", "display_type: html"),
        ("    format: { type: javascript, function: 'x => x' }\n", "not supported"),
        ("    format: { type: template, template: '/a/{{Other}}' }\n", "reads only {{value}}"),
        ("    display_type: sparkle\n", "x-"),
    ] {
        let e = presented(&root, &field(extra)).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(e.contains(says), "{extra}: {e}");
    }
    let e = presented(
        &root,
        "  - type: measure\n    name: N\n    data_type: integer\n    display_type: transcript\n    expression: count(*)\n",
    )
    .err()
    .map(|e| e.to_string())
    .unwrap_or_default();
    assert!(e.contains("renders text"), "{e}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Across every golden case, the columns a plan reports are the ones its
/// final SELECT names, each once and in order.
#[test]
fn every_planned_column_is_one_the_statement_selects() {
    let fx = Fixtures::open(Fixtures::default_root());
    let d = dialects();
    let mut models: std::collections::HashMap<String, Model> = Default::default();
    let mut checked = 0;
    for cases in fx.cases().expect("golden cases").into_values() {
        for case in cases {
            let model = models
                .entry(case.universe.clone())
                .or_insert_with(|| Model::build("g", &case.universe, fx.snapshot(&case.universe).expect("snapshot"), d).expect("universe"));
            let req = SqlRequest { spec: Some(case.spec.clone()), expr: None, context: case.context.clone() };
            let Ok(p) = model.plan_request(req, false, &unlimited) else { continue };
            let root = &p.plan.sql[p.plan.root_at..];
            let mut from = 0;
            for c in &p.plan.columns {
                let at = root[from..].find(&format!(" AS {}", quoted(&c.name))).map(|i| i + from);
                assert!(at.is_some(), "{}: column {:?} is not selected, in order, by\n{root}", case.id, c.name);
                from = at.unwrap() + 1;
            }
            checked += 1;
        }
    }
    assert!(checked > 100, "only {checked} cases planned");
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

//! Sources, datasets and runs, through the facade the server calls.

use super::*;
use crate::schema::ColumnType::*;
use serde_json::{json, Value};

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn prep() -> (Dir, Prep) {
    let dir = std::env::temp_dir().join(format!("zsql-prep-tests-{}-{}", std::process::id(), time::nanos()));
    std::fs::create_dir_all(&dir).unwrap();
    (Dir(dir.clone()), reopen(&dir))
}

fn reopen(dir: &Path) -> Prep {
    let mut settings = Settings::defaults(dir);
    settings.engine.memory = "256MB".into();
    settings.engine.threads = 1;
    Prep::open(dir, settings).unwrap()
}

fn scope() -> Scope {
    Scope { org: "default".into(), project: "shop".into() }
}

fn csv(text: &str) -> Upload<'_> {
    Upload { file_name: Some("budget.csv".into()), bytes: text.as_bytes(), options: CsvOptions::default() }
}

const BUDGET: &str = "region,month,amount\nEMEA,2026-01-01,100\nAPAC,2026-01-01,80\n";
const SALES: &str = "region,store,amount\nNY,a,10\nCA,b,5\nNY,c,7\n";

/// The fingerprint of a context, as the server makes it.
const ANN: &str = "context-of-ann";

fn json_rows(page: &Page) -> Vec<Value> {
    zsql_exec::json::rows(&page.rows).unwrap()
}

fn rows_of(prep: &Prep, scope: &Scope, id: &str) -> Vec<Value> {
    json_rows(&prep.page(scope, id, None, 0, 100, &View::default()).unwrap())
}

/// An upload source, and a dataset over it as it is.
fn upload(prep: &Prep, scope: &Scope, text: &str) -> (Source, Dataset) {
    let (s, _) = prep.create_upload(scope, Named::default(), csv(text)).unwrap();
    let d = prep.create_dataset(scope, Named::default(), Some(&s.id)).unwrap();
    (s, d)
}

fn source(prep: &Prep, scope: &Scope, text: &str) -> String {
    prep.create_upload(scope, Named::default(), csv(text)).unwrap().0.id
}

fn graph(v: Value) -> Graph {
    serde_json::from_value(json!({ "nodes": v })).unwrap()
}

/// A source, its steps, and the output.
fn steps_graph(src: &str, steps: Value) -> Graph {
    graph(json!([
        { "id": "in", "kind": "source", "source": src },
        { "id": "shape", "kind": "steps", "input": "in", "steps": steps },
        { "id": "out", "kind": "output", "input": "shape" },
    ]))
}

static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn run_id() -> String {
    format!("run-{}", RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// A run of `g` into dataset `ds`, its pulls none; refused before it
/// starts, the words of the refusal.
fn run(prep: &Prep, scope: &Scope, ds: &str, g: Graph) -> std::result::Result<RunRecord, String> {
    let request = RunRequest { id: run_id(), dataset: ds.into(), graph: g };
    match prep.begin(scope, request, ANN).map_err(|e| e.to_string())? {
        Begun::Started(t) => {
            assert!(t.pulls.is_empty(), "a run here pulls nothing: {:?}", t.pulls);
            Ok(prep.execute(*t))
        }
        Begun::Answered(r) => Ok(*r),
    }
}

/// A run that succeeds.
fn ran(prep: &Prep, scope: &Scope, ds: &str, g: Graph) -> RunRecord {
    let r = run(prep, scope, ds, g).unwrap();
    assert_eq!(r.state, RunState::Succeeded, "{}", r.summary);
    r
}

/// A new dataset of `text` shaped by `steps`: its id, or the words of
/// why the run did not make it.
fn shaped(prep: &Prep, scope: &Scope, text: &str, steps: Value) -> std::result::Result<(String, RunRecord), String> {
    let src = source(prep, scope, text);
    let d = prep.create_dataset(scope, Named::default(), None).unwrap();
    let r = run(prep, scope, &d.id, steps_graph(&src, steps))?;
    if r.state != RunState::Succeeded {
        return Err(r.summary);
    }
    Ok((d.id, r))
}

/// The output's columns.
fn columns_of(prep: &Prep, scope: &Scope, id: &str) -> Vec<Column> {
    prep.get(scope, id).unwrap().columns
}

/// What a preview of steps over a source says is wrong, in words.
fn refused(prep: &Prep, scope: &Scope, src: &str, steps: Value) -> String {
    let p = prep
        .preview(scope, &PreviewRequest { graph: steps_graph(src, steps), node: None, whole: false, limit: None }, ANN, &HashMap::new())
        .unwrap();
    p.problems.iter().map(Problem::said).collect::<Vec<_>>().join("; ")
}

fn shown(row: &Value) -> Vec<String> {
    row.as_array().unwrap().iter().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).collect()
}

// ------------------------------------------------- sources and datasets

#[test]
fn an_upload_is_a_source_and_a_dataset_over_it_copies_nothing() {
    let (dir, prep) = prep();
    let ann = scope();
    let (s, d) = upload(&prep, &ann, BUDGET);
    assert!(is_source_id(&s.id) && is_id(&d.id));
    assert_eq!((s.name.as_str(), s.kind, s.snapshot), ("budget", Kind::Upload, 1));
    assert_eq!((d.name.as_str(), d.live, d.rows), ("budget", Some(1), 2));
    let types: Vec<(&str, ColumnType, Role)> = d.columns.iter().map(|c| (c.name.as_str(), c.ty, c.role)).collect();
    assert_eq!(types, [("region", Text, Role::Dimension), ("month", Date, Role::Dimension), ("amount", Integer, Role::Measure)]);

    let root = dir.0.join("default/shop").join(DIR);
    for f in
        [format!(".sources/{}/source.json", s.id), format!(".sources/{}/snapshots/1/rows.arrow", s.id), format!("{}/dataset.json", d.id)]
    {
        assert!(root.join(&f).exists(), "{f}");
    }
    assert!(root.join(&d.id).join("v1/manifest.json").exists());
    assert!(!root.join(&d.id).join("v1/rows.arrow").exists(), "the output is the snapshot: no rows are copied");
    let page = prep.page(&ann, &d.id, None, 0, 10, &View::default()).unwrap();
    assert_eq!((page.total, json_rows(&page)[0].clone()), (2, json!(["EMEA", "2026-01-01", 100])));
    assert_eq!(json_rows(&prep.page(&ann, &d.id, None, 1, 10, &View::default()).unwrap()).len(), 1);

    let lineage = prep.lineage(&ann, &d.id, None).unwrap();
    assert_eq!((lineage.sources[0].source.as_str(), lineage.sources[0].snapshot), (s.id.as_str(), 1));
    assert_eq!(lineage.sources[0].file_name.as_deref(), Some("budget.csv"));
    assert_eq!(lineage.columns[2].from[0].column, "amount");
    let v = prep.version(&ann, &d.id, None).unwrap();
    assert_eq!(v.graph.unwrap().nodes.len(), 2, "the graph it is: a source and the output");
    assert_eq!(prep.sources(&ann).len(), 1);
    assert_eq!(prep.list(&ann).len(), 1);
}

#[test]
fn a_dataset_and_a_source_are_their_projects() {
    let (_dir, prep) = prep();
    let (s, d) = upload(&prep, &scope(), BUDGET);
    let elsewhere = Scope { project: "other".into(), ..scope() };
    assert!(matches!(prep.get(&elsewhere, &d.id), Err(Error::NotFound(_))));
    assert!(matches!(prep.source(&elsewhere, &s.id), Err(Error::NotFound(_))));
    assert!(prep.list(&elsewhere).is_empty() && prep.sources(&elsewhere).is_empty());
    let other_org = Scope { org: "acme".into(), ..scope() };
    assert!(matches!(prep.get(&other_org, &d.id), Err(Error::NotFound(_))));
    // An id is checked before it names a directory.
    assert!(matches!(prep.get(&scope(), "../../etc"), Err(Error::NotFound(_))));
    assert!(matches!(prep.source(&scope(), "src_../x"), Err(Error::NotFound(_))));
    assert!(matches!(prep.run(&scope(), "../x"), Err(Error::NotFound(_))));
}

#[test]
fn a_new_snapshot_runs_nothing_until_a_run_reads_it() {
    let (_dir, prep) = prep();
    let ann = scope();
    let (s, d) = upload(&prep, &ann, BUDGET);
    let (s2, snap) = prep.add_upload(&ann, &s.id, csv("region,month,amount,owner\nEMEA,2026-01-01,100.5,kim\n")).unwrap();
    assert_eq!((s2.snapshot, snap.number), (2, 2));
    assert!(snap.changes.iter().any(|n| n.contains("widened")), "{:?}", snap.changes);
    assert!(snap.changes.iter().any(|n| n.contains("'owner' is new")));
    assert_eq!(prep.get(&ann, &d.id).unwrap().live, Some(1), "nothing ran");
    assert_eq!(rows_of(&prep, &ann, &d.id).len(), 2);
    // The application runs it: the newest snapshot, as it is.
    let r = ran(&prep, &ann, &d.id, prep.version(&ann, &d.id, None).unwrap().graph.unwrap());
    assert_eq!(r.version, Some(2));
    let d = prep.get(&ann, &d.id).unwrap();
    assert_eq!((d.live, d.rows, d.columns[2].ty), (Some(2), 1, Decimal(1)));
    assert_eq!(prep.lineage(&ann, &d.id, None).unwrap().sources[0].snapshot, 2);
    // Every version is still there.
    assert_eq!(prep.page(&ann, &d.id, Some(1), 0, 10, &View::default()).unwrap().total, 2);
    // A snapshot whose values will not convert is refused, and changes nothing.
    let e = prep.add_upload(&ann, &s.id, csv("region,month,amount\nEMEA,soon,1\n")).err().unwrap();
    assert!(matches!(e, Error::Unprocessable(_)) && e.to_string().contains("'month' is date"), "{e}");
    assert_eq!(prep.source(&ann, &s.id).unwrap().snapshot, 2);
    // A source takes only its kind of snapshot.
    assert!(prep.add_push(&ann, &s.id, Pushed { rows: vec![], origin: None }).is_err());
}

#[test]
fn the_caps_hold_and_a_failure_leaves_nothing() {
    let (dir, _) = prep();
    let mut settings = Settings::defaults(&dir.0);
    settings.max_rows = 1;
    let prep = Prep::open(&dir.0, settings).unwrap();
    let e = prep.create_upload(&scope(), Named::default(), csv(BUDGET)).err().unwrap();
    assert!(e.to_string().contains("more than 1 rows"), "{e}");
    let mut settings = Settings::defaults(&dir.0);
    settings.max_bytes = 10;
    let prep = Prep::open(&dir.0, settings).unwrap();
    assert!(prep.create_upload(&scope(), Named::default(), csv(BUDGET)).err().unwrap().to_string().contains("bytes"));
    let sources = dir.0.join("default/shop").join(DIR).join(store::SOURCES);
    assert_eq!(std::fs::read_dir(&sources).map(|r| r.count()).unwrap_or(0), 0);
}

#[test]
fn deletes_are_hard_and_a_source_gone_leaves_its_datasets_answering() {
    let (dir, prep) = prep();
    let ann = scope();
    let (s, d) = upload(&prep, &ann, BUDGET);
    let other = prep.create_dataset(&ann, Named::default(), None).unwrap();
    ran(&prep, &ann, &other.id, steps_graph(&s.id, json!([{ "op": "filter", "formula": "amount > 90" }])));
    // The source goes: the dataset over it as it was gets its rows.
    assert_eq!(prep.delete_source(&ann, &s.id).unwrap(), std::slice::from_ref(&d.id));
    assert!(prep.source(&ann, &s.id).is_err());
    assert_eq!(rows_of(&prep, &ann, &d.id).len(), 2, "it still answers");
    assert_eq!(rows_of(&prep, &ann, &other.id).len(), 1);
    // A run that reads it fails, saying so.
    let e = run(&prep, &ann, &other.id, steps_graph(&s.id, json!([]))).err().unwrap();
    assert!(e.contains("node in: it reads source") && e.contains("not in this project"), "{e}");
    prep.delete(&ann, &d.id).unwrap();
    assert!(!dir.0.join("default/shop").join(DIR).join(&d.id).exists());
    let elsewhere = Scope { project: "other".into(), ..scope() };
    let (_, kept) = upload(&prep, &elsewhere, BUDGET);
    let removed = prep.delete_project("default", "shop").unwrap();
    assert_eq!(removed, [other.id]);
    assert!(!dir.0.join("default/shop").join(DIR).exists());
    assert!(prep.get(&elsewhere, &kept.id).is_ok(), "another project's stays");
}

#[test]
fn pushed_rows_are_a_source_and_say_where_they_came_from() {
    let (_dir, prep) = prep();
    let ann = scope();
    let rows: Vec<serde_json::Map<String, Value>> = serde_json::from_value(json!([
        { "deal": "Acme", "value": 1200, "won": true },
        { "deal": "Globex", "value": 99.5, "stage": "lost" },
    ]))
    .unwrap();
    let origin = json!({ "tool": "crm.deals", "at": "2026-11-27" });
    let named = Named { name: Some("deals".into()), description: Some("from the CRM".into()) };
    let (s, snap) = prep.create_push(&ann, named, Pushed { rows, origin: Some(origin.clone()) }).unwrap();
    assert_eq!((s.kind, s.description.as_deref(), snap.origin), (Kind::Push, Some("from the CRM"), Some(origin)));
    let types: Vec<(&str, ColumnType)> = snap.columns.iter().map(|c| (c.name.as_str(), c.ty)).collect();
    assert_eq!(types, [("deal", Text), ("value", Decimal(1)), ("won", Boolean), ("stage", Text)]);
    let page = prep.source_rows(&ann, &s.id, None, 0, 10, &View::default()).unwrap();
    assert_eq!(json_rows(&page)[1], json!(["Globex", "99.5", null, "lost"]));
    let bad: Vec<serde_json::Map<String, Value>> = serde_json::from_value(json!([{ "a": 1, "A": 2 }])).unwrap();
    assert!(prep.add_push(&ann, &s.id, Pushed { rows: bad, origin: None }).err().unwrap().to_string().contains("but for case"));
}

// ---------------------------------------------------------------- steps

#[test]
fn a_steps_node_makes_each_version_and_says_what_each_step_did() {
    let (_dir, prep) = prep();
    let ann = scope();
    let (id, r) = shaped(
        &prep,
        &ann,
        SALES,
        json!([
            { "op": "remap_val", "column": "region", "values": [{ "from": "NY", "to": "New York" }] },
            { "op": "group", "by": ["region"], "aggregates": [{ "column": "amount", "fn": "sum", "as": "total" }] },
        ]),
    )
    .unwrap();
    assert_eq!(rows_of(&prep, &ann, &id), [json!(["CA", 5]), json!(["New York", 17])]);
    // Sorted and filtered for viewing, which changes nothing kept.
    let sort = [SortKey { column: "total".into(), desc: true }];
    let page = prep.page(&ann, &id, None, 0, 10, &View { sort: &sort, filter: Some("total > 6") }).unwrap();
    assert_eq!((page.total, json_rows(&page)), (1, vec![json!(["New York", 17])]));
    // Lineage follows each column through the steps.
    assert_eq!(prep.lineage(&ann, &id, None).unwrap().columns[1].from[0].column, "amount");
    // The record says what each node did.
    let shape = r.nodes.iter().find(|n| n.id == "shape").unwrap();
    assert_eq!((shape.rows_in.clone(), shape.rows), (vec![3], 2));
    assert_eq!(shape.steps[0].changed, Some(2));
    assert_eq!(shape.steps[0].said, "remapped 2 value(s) of region");
    assert_eq!(shape.steps[1].said, "grouped 3 rows into 2");
    assert_eq!(shape.summary, "2 steps: 3 rows in, 2 out; remapped 2 value(s) of region; grouped 3 rows into 2.");
    assert_eq!(shape.added, ["total"]);
    assert_eq!(shape.removed, ["store", "amount"]);
    let input = r.nodes.iter().find(|n| n.id == "in").unwrap();
    assert!(input.summary.starts_with("Read budget.csv (source budget, snapshot 1"), "{}", input.summary);
    let total = shape.profile.iter().find(|p| p.column == "total").unwrap();
    assert_eq!((total.nulls, total.distinct, total.min.as_deref(), total.max.as_deref()), (0, 2, Some("5"), Some("17")));
    assert!(r.summary.starts_with("Made version 1: 2 rows, 2 columns, from budget"), "{}", r.summary);
    let v = prep.version(&ann, &id, Some(1)).unwrap();
    assert_eq!(v.run.unwrap().id, r.id);
    assert_eq!(v.graph.unwrap().nodes.len(), 3);
}

#[test]
fn an_unpivot_makes_month_columns_rows_and_takes_a_new_month_in_a_later_file() {
    let (_dir, prep) = prep();
    let ann = scope();
    let src = source(&prep, &ann, "dept,Jan,Feb,Mar\nGROCERY,100,110,\nPRODUCE,40,,45\n");
    let d = prep.create_dataset(&ann, Named::default(), None).unwrap();
    let unpivot = json!([{ "op": "unpivot", "keep": ["dept"], "name": "month", "value": "budget" }]);
    ran(&prep, &ann, &d.id, steps_graph(&src, unpivot.clone()));
    let names: Vec<(String, ColumnType, Role)> = columns_of(&prep, &ann, &d.id).into_iter().map(|c| (c.name, c.ty, c.role)).collect();
    assert_eq!(
        names,
        [("dept".into(), Text, Role::Dimension), ("month".into(), Text, Role::Dimension), ("budget".into(), Integer, Role::Measure)]
    );
    assert_eq!(
        rows_of(&prep, &ann, &d.id),
        [json!(["GROCERY", "Jan", 100]), json!(["GROCERY", "Feb", 110]), json!(["PRODUCE", "Jan", 40]), json!(["PRODUCE", "Mar", 45])],
        "a row per month, in order, and no row for an empty value"
    );
    // Kept by naming what stays: April, new in the next file, is unpivoted too.
    prep.add_upload(&ann, &src, csv("dept,Jan,Apr\nGROCERY,1,2.5\n")).unwrap();
    ran(&prep, &ann, &d.id, steps_graph(&src, unpivot));
    assert_eq!(rows_of(&prep, &ann, &d.id), [json!(["GROCERY", "Jan", "1.0"]), json!(["GROCERY", "Apr", "2.5"])]);
    // Empty values kept, when asked; columns named instead.
    prep.add_upload(&ann, &src, csv("dept,Jan,Apr\nGROCERY,,3\n")).unwrap();
    let named = json!([{ "op": "unpivot", "columns": ["Jan"], "name": "month", "value": "budget", "keep_empty": true }]);
    ran(&prep, &ann, &d.id, steps_graph(&src, named));
    assert_eq!(rows_of(&prep, &ann, &d.id), [json!(["GROCERY", "3.0", "Jan", null])], "Apr stays the decimal it was");
    for (step, says) in [
        (json!({ "op": "unpivot", "columns": ["Jan"], "keep": ["dept"], "name": "m", "value": "v" }), "and not both"),
        (json!({ "op": "unpivot", "columns": ["Jan"], "name": "dept", "value": "v" }), "there is a column 'dept' already"),
        (json!({ "op": "unpivot", "columns": ["dept", "Apr"], "name": "m", "value": "v" }), "do not widen to one type"),
        (json!({ "op": "unpivot", "keep": ["dept", "Jan", "Apr"], "name": "m", "value": "v" }), "nothing to unpivot"),
    ] {
        let e = refused(&prep, &ann, &src, json!([step]));
        assert!(e.contains(says) && e.starts_with("node shape: step 1 (unpivot)"), "{says}: {e}");
    }
}

#[test]
fn update_column_computes_a_column_again_and_takes_its_formulas_type() {
    let (_dir, prep) = prep();
    let ann = scope();
    let update = json!([
        { "op": "update_column", "column": "name", "formula": "upper(trim(name))" },
        { "op": "update_column", "column": "price", "formula": "round(price, 1)" },
        { "op": "add_column", "column": "label", "formula": "name & ' ' & to_text(price)" },
        { "op": "update_column", "column": "price", "formula": "if(price > 2, 'dear', 'cheap')" },
    ]);
    let (id, r) = shaped(&prep, &ann, "name,price\n  Milk ,1.234\nEggs,2.5\n", update).unwrap();
    assert_eq!(rows_of(&prep, &ann, &id), [json!(["MILK", "cheap", "MILK 1.2"]), json!(["EGGS", "dear", "EGGS 2.5"])]);
    let price = columns_of(&prep, &ann, &id).into_iter().find(|c| c.name == "price").unwrap();
    assert_eq!((price.ty, price.role), (Text, Role::Dimension), "a measure made text is a dimension");
    let shape = &r.nodes[1];
    assert_eq!(shape.steps[0].said, "computed name again: 2 value(s) changed");
    assert_eq!(shape.retyped.len(), 1, "price, from a decimal to text");
    let src = source(&prep, &ann, "name\nx\n");
    let e = refused(&prep, &ann, &src, json!([{ "op": "update_column", "column": "gone", "formula": "1" }]));
    assert_eq!(e, "node shape: step 1 (update_column) reads column 'gone', which is not there");
}

#[test]
fn to_number_reads_numbers_as_people_write_them() {
    let (_dir, prep) = prep();
    let ann = scope();
    let file = "v\n1234.5\n\"$1,234.50\"\n(500)\n12%\n(12.5%)\nUSD 10\n10 EUR\n1 234\n1e3\n\"€ 9,99\"\n1.5\n12 apples\n1.2.3\n";
    let add = json!([
        { "op": "add_column", "column": "n", "formula": "to_number(v)" },
        { "op": "add_column", "column": "eu", "formula": "to_number(v, ',')" },
        { "op": "add_column", "column": "i", "formula": "to_integer(v)" },
        { "op": "add_column", "column": "ok", "formula": "is_number(v)" },
    ]);
    let (id, _) = shaped(&prep, &ann, file, add).unwrap();
    assert_eq!(columns_of(&prep, &ann, &id)[1].ty, Decimal(6));
    let want = [
        ["1234.5", "1234.500000", "12345.000000", "null", "true"],
        ["$1,234.50", "1234.500000", "1.234500", "null", "true"],
        ["(500)", "-500.000000", "-500.000000", "-500", "true"],
        ["12%", "0.120000", "0.120000", "null", "true"],
        ["(12.5%)", "-0.125000", "-1.250000", "null", "true"],
        ["USD 10", "10.000000", "10.000000", "10", "true"],
        ["10 EUR", "10.000000", "10.000000", "10", "true"],
        ["1 234", "1234.000000", "1234.000000", "1234", "true"],
        ["1e3", "1000.000000", "1000.000000", "1000", "true"],
        ["€ 9,99", "999.000000", "9.990000", "999", "true"],
        ["1.5", "1.500000", "15.000000", "null", "true"],
        ["12 apples", "null", "null", "null", "false"],
        ["1.2.3", "null", "123.000000", "null", "false"],
    ];
    for (row, w) in rows_of(&prep, &ann, &id).iter().zip(want) {
        assert_eq!(shown(row), w, "{}", w[0]);
    }
    let src = source(&prep, &ann, "v\n1\n");
    let e = refused(&prep, &ann, &src, json!([{ "op": "filter", "formula": "to_number(v, ';') > 1" }]));
    assert!(e.contains("decimal mark is '.' or ','"), "{e}");
}

#[test]
fn to_date_reads_every_unambiguous_way_a_date_is_written() {
    let (_dir, prep) = prep();
    let ann = scope();
    let file = "v\n2026-01-05\n2026/1/5\n2026.01.05\n2026-01-05T10:30:00Z\n20260105\n202601\n2026-01\n5 Jan 2026\n\"January 5, 2026\"\n05-jan-2026\nSeptember 2026\n31/01/2026\n01/31/2026\n46027\n2026-13-01\n202613\nsoon\n";
    let add = json!([
        { "op": "add_column", "column": "auto", "formula": "to_date(v)" },
        { "op": "add_column", "column": "dmy", "formula": "to_date(v, 'dmy')" },
        { "op": "add_column", "column": "mdy", "formula": "to_date(v, 'mdy')" },
        { "op": "add_column", "column": "excel", "formula": "to_date(v, 'excel')" },
        { "op": "add_column", "column": "fmt", "formula": "to_date(v, '%d/%m/%Y')" },
        { "op": "add_column", "column": "ok", "formula": "is_date(v)" },
    ]);
    let (id, _) = shaped(&prep, &ann, file, add).unwrap();
    let n = "null";
    let want = [
        ["2026-01-05", "2026-01-05", "2026-01-05", "2026-01-05", "2026-01-05", n, "true"],
        ["2026/1/5", "2026-01-05", "2026-01-05", "2026-01-05", "2026-01-05", n, "true"],
        ["2026.01.05", "2026-01-05", "2026-01-05", "2026-01-05", "2026-01-05", n, "true"],
        ["2026-01-05T10:30:00Z", "2026-01-05", "2026-01-05", "2026-01-05", "2026-01-05", n, "true"],
        ["20260105", "2026-01-05", "2026-01-05", "2026-01-05", "2026-01-05", n, "true"],
        ["202601", "2026-01-01", "2026-01-01", "2026-01-01", "2026-01-01", n, "true"],
        ["2026-01", "2026-01-01", "2026-01-01", "2026-01-01", "2026-01-01", n, "true"],
        ["5 Jan 2026", "2026-01-05", "2026-01-05", "2026-01-05", "2026-01-05", n, "true"],
        ["January 5, 2026", "2026-01-05", "2026-01-05", "2026-01-05", "2026-01-05", n, "true"],
        ["05-jan-2026", "2026-01-05", "2026-01-05", "2026-01-05", "2026-01-05", n, "true"],
        ["September 2026", "2026-09-01", "2026-09-01", "2026-09-01", "2026-09-01", n, "true"],
        ["31/01/2026", n, "2026-01-31", n, n, "2026-01-31", "false"],
        ["01/31/2026", n, n, "2026-01-31", n, n, "false"],
        ["46027", n, n, n, "2026-01-05", n, "false"],
        ["2026-13-01", n, n, n, n, n, "false"],
        ["202613", n, n, n, n, n, "false"],
        ["soon", n, n, n, n, n, "false"],
    ];
    let got = rows_of(&prep, &ann, &id);
    assert_eq!(got.len(), want.len());
    for (row, w) in got.iter().zip(want) {
        assert_eq!(shown(row), w, "{}", w[0]);
    }
    let src = source(&prep, &ann, "v\nx\n");
    let e = refused(&prep, &ann, &src, json!([{ "op": "filter", "formula": "is_date(v, 'ydm')" }]));
    assert!(e.contains("'dmy' (31/01/2026)"), "{e}");
}

#[test]
fn switch_ifs_and_the_boolean_and_timestamp_readers() {
    let (_dir, prep) = prep();
    let ann = scope();
    let file = "region,amount,flag,at\nNY,150,Yes,2026-01-05 10:30\nCA,50,n,31/01/2026 14:05\nTX,5,maybe,46027.5\n,20,1,5 Jan 2026\n";
    let add = json!([
        { "op": "add_column", "column": "name", "formula": "switch(region, 'NY', 'New York', 'CA', 'California', region)" },
        { "op": "add_column", "column": "no_default", "formula": "switch(region, 'NY', 1)" },
        { "op": "add_column", "column": "size", "formula": "ifs(amount > 100, 'big', amount > 10, 'mid', 'small')" },
        { "op": "add_column", "column": "yes", "formula": "to_boolean(flag)" },
        { "op": "add_column", "column": "auto", "formula": "to_timestamp(at)" },
        { "op": "add_column", "column": "dmy", "formula": "to_timestamp(at, 'dmy')" },
        { "op": "add_column", "column": "excel", "formula": "to_timestamp(at, 'excel')" },
        { "op": "remove_columns", "columns": ["region", "amount", "flag", "at"] },
    ]);
    let (id, _) = shaped(&prep, &ann, file, add).unwrap();
    let types: Vec<ColumnType> = columns_of(&prep, &ann, &id).iter().map(|c| c.ty).collect();
    assert_eq!(types, [Text, Integer, Text, Boolean, Timestamp, Timestamp, Timestamp]);
    let n = Value::Null;
    assert_eq!(
        rows_of(&prep, &ann, &id),
        [
            json!(["New York", 1, "big", true, "2026-01-05T10:30:00", "2026-01-05T10:30:00", "2026-01-05T10:30:00"]),
            json!(["California", n, "mid", false, n, "2026-01-31T14:05:00", n]),
            json!(["TX", n, "small", n, n, n, "2026-01-05T12:00:00"]),
            json!([n, n, "mid", true, "2026-01-05T00:00:00", "2026-01-05T00:00:00", "2026-01-05T00:00:00"]),
        ]
    );
    let src = source(&prep, &ann, "region,amount\nNY,1\n");
    for (formula, says) in [
        ("switch(region, 'NY')", "switch() takes a value, then pairs"),
        ("switch(region, 1, 'x')", "switch()'s values meets text with integer"),
        ("ifs(amount, 'x')", "ifs()'s conditions takes boolean"),
        ("switch(region, 'NY', 1, 'CA', 'x')", "switch()'s results"),
    ] {
        let e = refused(&prep, &ann, &src, json!([{ "op": "add_column", "column": "k", "formula": formula }]));
        assert!(e.contains(says), "{formula}: {e}");
    }
}

#[test]
fn a_sql_step_is_a_black_box_over_input_alone() {
    let (_dir, prep) = prep();
    let ann = scope();
    let file = "region,store,month,amount\nNY,a,2026-01,10\nNY,b,2026-01,5\nNY,a,2026-02,7\nCA,c,2026-01,3\n";
    let src = source(&prep, &ann, file);
    let d = prep.create_dataset(&ann, Named::default(), None).unwrap();
    let q = "SELECT region, month, sum(amount) AS total, \
             sum(sum(amount)) OVER (PARTITION BY region ORDER BY month) AS running, \
             regexp_extract(month, '-(\\d+)', 1) AS m \
             FROM input GROUP BY region, month ORDER BY region, month;";
    let steps = json!([
        { "op": "remap_val", "column": "region", "values": [{ "from": "NY", "to": "New York" }] },
        { "op": "sql", "query": q },
        { "op": "add_column", "column": "share", "formula": "total / running" },
    ]);
    ran(&prep, &ann, &d.id, steps_graph(&src, steps.clone()));
    let shape: Vec<(String, ColumnType, Role)> = columns_of(&prep, &ann, &d.id).into_iter().map(|c| (c.name, c.ty, c.role)).collect();
    assert_eq!(
        shape,
        [
            ("region".into(), Text, Role::Dimension),
            ("month".into(), Text, Role::Dimension),
            ("total".into(), Integer, Role::Measure),
            ("running".into(), Integer, Role::Measure),
            ("m".into(), Text, Role::Dimension),
            ("share".into(), Double, Role::Measure),
        ]
    );
    assert_eq!(
        rows_of(&prep, &ann, &d.id),
        [
            json!(["CA", "2026-01", 3, 3, "01", 1.0]),
            json!(["New York", "2026-01", 15, 15, "01", 1.0]),
            json!(["New York", "2026-02", 7, 22, "02", 7.0 / 22.0])
        ],
        "it reads the step before it: NY was remapped first"
    );
    // A column it reads, gone upstream, refuses the run, naming it and what follows.
    prep.add_upload(&ann, &src, csv("region,store,amount\nNY,a,1\n")).unwrap();
    let request = RunRequest { id: run_id(), dataset: d.id.clone(), graph: steps_graph(&src, steps) };
    let Err(Error::Problems(problems)) = prep.begin(&ann, request, ANN) else { panic!("refused") };
    let said: Vec<String> = problems.iter().map(Problem::said).collect();
    assert_eq!(
        said,
        [
            "node shape: step 2 (sql) reads column 'month', which is not there",
            "node shape: step 3 (add_column) reads column 'running', which is not there",
            "node shape: step 3 (add_column) reads column 'total', which is not there",
        ]
    );
    assert_eq!(prep.get(&ann, &d.id).unwrap().live, Some(1), "what was live stays live");
    for (query, says) in [
        ("SELECT * FROM s1", "s1"),
        ("SELECT * FROM shape_t1", "shape_t1"),
        ("SELECT * FROM read_csv('/etc/passwd')", "read_csv"),
        ("SELECT getenv('HOME') AS h", "getenv"),
        ("SELECT * FROM input; DROP TABLE input", "one query"),
        ("SELECT *, now() AS at FROM input", "now is not the same on every run"),
        ("SELECT [1, 2] AS l FROM input", "CAST it to text"),
    ] {
        let e = refused(&prep, &ann, &src, json!([{ "op": "sql", "query": query }]));
        assert!(e.contains("step 1 (sql)") && e.contains(says), "{query}: {e}");
    }
    // Names are DuckDB's: a repeat is numbered, an expression is its text.
    let g = steps_graph(&src, json!([{ "op": "sql", "query": "SELECT region, region, upper(region) FROM input" }]));
    let p = prep.preview(&ann, &PreviewRequest { graph: g, node: None, whole: false, limit: None }, ANN, &HashMap::new()).unwrap();
    assert_eq!(p.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["region", "region_1", "upper(region)"]);
}

#[test]
fn a_sql_steps_types_are_ones_a_dataset_holds() {
    let (_dir, prep) = prep();
    let ann = scope();
    let q = "SELECT big, CAST(a AS DECIMAL(38,20)) AS fine, sum(1) OVER () AS n FROM input";
    let (id, _) =
        shaped(&prep, &ann, "big,a\n12345678901234567890,1.5\n98765432109876543210,2.5\n", json!([{ "op": "sql", "query": q }])).unwrap();
    let types: Vec<(String, ColumnType)> = columns_of(&prep, &ann, &id).into_iter().map(|c| (c.name, c.ty)).collect();
    assert_eq!(
        types,
        [("big".into(), Decimal(0)), ("fine".into(), Double), ("n".into(), Integer)],
        "20 digits stay a decimal, more places than 18 are a float, and a sum of integers is one"
    );
    assert_eq!(rows_of(&prep, &ann, &id).len(), 2);
}

#[test]
fn a_formula_that_would_make_a_huge_value_is_refused_and_double_equals_compares() {
    let (_dir, prep) = prep();
    let ann = scope();
    let src = source(&prep, &ann, "region,amount\nNY,1\nCA,2\n");
    let nested = (0..6).fold("region".to_string(), |x, _| format!("replace({x}, 'N', 'NNNNNNNNNNNNNNNN')"));
    let d = prep.create_dataset(&ann, Named::default(), None).unwrap();
    let r = run(&prep, &ann, &d.id, steps_graph(&src, json!([{ "op": "add_column", "column": "big", "formula": nested }]))).unwrap();
    assert!(r.summary.contains("replace() would make a value longer than"), "{}", r.summary);
    ran(
        &prep,
        &ann,
        &d.id,
        steps_graph(
            &src,
            json!([
                { "op": "add_column", "column": "ny", "formula": "region == 'NY'" },
                { "op": "add_column", "column": "r", "formula": "replace(replace(region, 'N', 'n'), 'Y', 'y')" },
            ]),
        ),
    );
    assert_eq!(rows_of(&prep, &ann, &d.id), [json!(["NY", 1, true, "ny"]), json!(["CA", 2, false, "CA"])]);
}

#[test]
fn a_file_too_wide_or_of_too_long_a_field_is_refused_as_it_is_read() {
    let (_dir, prep) = prep();
    let ann = scope();
    let wide = format!("{}\n", vec!["a"; crate::source::COLUMN_LIMIT + 1].join(","));
    let e = prep.create_upload(&ann, Named::default(), csv(&wide)).err().unwrap();
    assert!(e.to_string().contains("columns a dataset has"), "{e}");
    let long = format!("a,b\n{},1\n", "x".repeat(crate::source::FIELD_LIMIT + 1));
    let e = prep.create_upload(&ann, Named::default(), csv(&long)).err().unwrap();
    assert!(e.to_string().contains("more than the 1048576 a value is"), "{e}");
}

#[test]
fn case_steps_rewrite_text_values() {
    let (_dir, prep) = prep();
    let ann = scope();
    let cases = json!([
        { "op": "upper_case", "columns": ["a"] },
        { "op": "lower_case", "columns": ["b"] },
        { "op": "title_case", "columns": ["c"] },
        { "op": "snake_case", "columns": ["d", "e"] },
    ]);
    let file = "a,b,c,d,e\ngrocery store,Grocery Store,GroceryStore,order-ID 2,  Ünïcode wörd \n";
    let (id, _) = shaped(&prep, &ann, file, cases).unwrap();
    assert_eq!(rows_of(&prep, &ann, &id), [json!(["GROCERY STORE", "grocery store", "Grocerystore", "order_id_2", "ünïcode_wörd"])]);
    let camel = json!([{ "op": "camel_case", "columns": ["a", "b", "c", "d", "e"] }]);
    let (id, _) = shaped(&prep, &ann, file, camel).unwrap();
    assert_eq!(rows_of(&prep, &ann, &id), [json!(["groceryStore", "groceryStore", "groceryStore", "orderId2", "ünïcodeWörd"])]);
    let nested = json!([
        { "op": "add_column", "column": "key", "formula": "snake_case(trim(a)) & '_' & to_text(e)" },
        { "op": "add_column", "column": "camel", "formula": "camel_case(a)" },
        { "op": "add_column", "column": "title", "formula": "title_case(lower(trim(a)))" },
        { "op": "remove_columns", "columns": ["a", "b", "c", "d", "e"] },
    ]);
    let (id, _) = shaped(&prep, &ann, "a,b,c,d,e\n  Grocery Store ,x,y,z,2026\n", nested).unwrap();
    assert_eq!(rows_of(&prep, &ann, &id), [json!(["grocery_store_2026", "groceryStore", "Grocery Store"])]);
    let src = source(&prep, &ann, "a\nx\n");
    assert!(refused(&prep, &ann, &src, json!([{ "op": "add_column", "column": "k", "formula": "snake_case(1)" }])).contains("snake_case()"));
}

#[test]
fn fill_down_carries_a_label_to_the_rows_below() {
    let (_dir, prep) = prep();
    let ann = scope();
    let fill = json!([
        { "op": "find_and_replace_val", "column": "item", "find": "eggs", "with": " ", "whole": true },
        { "op": "find_and_replace_val", "column": "item", "find": "bread", "with": " bread ", "whole": true },
        { "op": "fill_down", "columns": ["dept", "item", "qty"] },
    ]);
    let (id, r) = shaped(&prep, &ann, "dept,item,qty\nGROCERY,milk,1\n,eggs,\n,bread,3\nPRODUCE,apples,4\n,pears,5\n", fill).unwrap();
    assert_eq!(
        rows_of(&prep, &ann, &id),
        [
            json!(["GROCERY", "milk", 1]),
            json!(["GROCERY", "milk", 1]),
            json!(["GROCERY", " bread ", 3]),
            json!(["PRODUCE", "apples", 4]),
            json!(["PRODUCE", "pears", 5])
        ],
        "an empty value, or blank text, is the one above it; a value there is kept as it is"
    );
    assert_eq!(r.nodes[1].steps[2].said, "filled 3 blank(s) in dept, item, qty from the row above");
}

#[test]
fn a_column_gone_upstream_fails_the_run_naming_it_with_what_it_likely_became() {
    let (_dir, prep) = prep();
    let ann = scope();
    let src = source(&prep, &ann, "region,amount\nNY,1\nCA,2\nTX,3\n");
    let d = prep.create_dataset(&ann, Named::default(), None).unwrap();
    let steps = json!([{ "op": "add_column", "column": "label", "formula": "upper(region)" }]);
    ran(&prep, &ann, &d.id, steps_graph(&src, steps.clone()));
    // `region` is gone; `Area` is new and has its values.
    prep.add_upload(&ann, &src, csv("Area,amount,regoin\nNY,1,x\nCA,2,y\n")).unwrap();
    let request = RunRequest { id: run_id(), dataset: d.id.clone(), graph: steps_graph(&src, steps.clone()) };
    let Err(Error::Problems(problems)) = prep.begin(&ann, request, ANN) else { panic!("refused") };
    let p = &problems[0];
    assert_eq!((p.node.as_deref(), p.code, p.column.as_deref()), (Some("shape"), graph::Code::MissingColumn, Some("region")));
    let s: Vec<&str> = p.suggestions.iter().map(|s| s.column.as_str()).collect();
    assert_eq!(s, ["Area", "regoin"], "by its values first, then by its name");
    assert!(p.suggestions[0].because.contains("67% of the values 'region' had are in it"), "{}", p.suggestions[0].because);
    assert_eq!(prep.get(&ann, &d.id).unwrap().live, Some(1));
    // The application points the step at the new name, and it runs.
    ran(&prep, &ann, &d.id, steps_graph(&src, json!([{ "op": "add_column", "column": "label", "formula": "upper(Area)" }])));
    // A dry run says the same as the run, before it.
    let c = prep.check(&ann, &steps_graph(&src, steps), ANN).unwrap();
    assert!(!c.ok && c.said.starts_with("It would not run: node shape: step 1 (add_column) reads column 'region'"), "{}", c.said);
}

#[test]
fn a_preview_stops_at_a_node_over_a_sample_and_keeps_nothing() {
    let (_dir, prep) = prep();
    let ann = scope();
    let (s, d) = upload(&prep, &ann, SALES);
    let g = graph(json!([
        { "id": "in", "kind": "source", "source": s.id },
        { "id": "big", "kind": "steps", "input": "in", "steps": [{ "op": "filter", "formula": "amount > 6" }] },
        { "id": "never", "kind": "steps", "input": "big", "steps": [{ "op": "trim_val", "columns": ["gone"] }] },
    ]));
    let p = prep
        .preview(&ann, &PreviewRequest { graph: g.clone(), node: Some("big".into()), whole: false, limit: None }, ANN, &HashMap::new())
        .unwrap();
    assert_eq!((p.rows.num_rows(), p.node.as_str(), p.sample), (2, "big", Some(1000)));
    assert!(p.problems.is_empty(), "what is past the node is not compiled: {:?}", p.problems);
    assert_eq!(p.nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), ["in", "big"]);
    assert_eq!(p.reads[0].id, s.id);
    let p = prep
        .preview(&ann, &PreviewRequest { graph: g, node: Some("never".into()), whole: true, limit: None }, ANN, &HashMap::new())
        .unwrap();
    assert_eq!((p.problems.len(), p.sample), (1, None), "a preview shows what will not resolve rather than refusing");
    assert_eq!(prep.get(&ann, &d.id).unwrap().made, 1, "nothing was made");
    let e = refused(&prep, &ann, &s.id, json!([{ "op": "retype_column", "column": "region", "type": "integer" }]));
    assert!(e.contains("do not convert without loss, such as 'NY'"), "{e}");
}

// -------------------------------------------------- joins, appends, sql

#[test]
fn a_join_node_brings_the_right_sides_columns_and_refuses_to_multiply_rows() {
    let (_dir, prep) = prep();
    let ann = scope();
    let sales = source(&prep, &ann, "region,amount\nNY,10\nCA,5\nTX,1\n");
    let (owners, owners_ds) = upload(&prep, &ann, "area,manager\nNY,kim\nCA,lee\n,ann\n,bo\n");
    let d = prep.create_dataset(&ann, Named::default(), None).unwrap();
    let joined = |right: Value, how: &str| {
        graph(json!([
            { "id": "sales", "kind": "source", "source": sales },
            right,
            { "id": "j", "kind": "join", "left": "sales", "right": "owners", "on": [{ "left": "region", "right": "area" }], "type": how },
            { "id": "out", "kind": "output", "input": "j" },
        ]))
    };
    // The other side may be a dataset's output, or a source.
    let r = ran(&prep, &ann, &d.id, joined(json!({ "id": "owners", "kind": "dataset", "dataset": owners_ds.id }), "left"));
    assert_eq!(
        rows_of(&prep, &ann, &d.id),
        [json!(["NY", 10, "kim"]), json!(["CA", 5, "lee"]), json!(["TX", 1, null])],
        "null keys repeat nothing"
    );
    let j = r.nodes.iter().find(|n| n.id == "j").unwrap();
    assert_eq!(
        (j.join.as_ref().unwrap().matched, j.join.as_ref().unwrap().unmatched_left, j.join.as_ref().unwrap().unmatched_right),
        (2, 1, 2)
    );
    assert_eq!(j.summary, "Joined owners: 2 of 3 rows matched; 1 have no match and are kept.");
    let lineage = prep.lineage(&ann, &d.id, None).unwrap();
    assert_eq!((lineage.datasets[0].id.as_str(), lineage.datasets[0].node.as_deref()), (owners_ds.id.as_str(), Some("owners")));
    assert_eq!(lineage.columns[2].from[0].source, format!("dataset:{}", owners_ds.id));
    assert_eq!(r.reads.iter().map(|x| x.kind.as_str()).collect::<Vec<_>>(), ["source", "dataset"]);
    let r = ran(&prep, &ann, &d.id, joined(json!({ "id": "owners", "kind": "source", "source": owners.id }), "inner"));
    assert_eq!(prep.get(&ann, &d.id).unwrap().rows, 2);
    assert!(r.nodes[2].summary.contains("are dropped"), "{}", r.nodes[2].summary);
    // A side that repeats a key would multiply rows: the run fails at the join.
    let dup = source(&prep, &ann, "area,manager\nNY,kim\nNY,ann\n");
    let r = run(&prep, &ann, &d.id, joined(json!({ "id": "owners", "kind": "source", "source": dup }), "left")).unwrap();
    assert_eq!((r.state, r.problems[0].node.as_deref(), r.problems[0].code), (RunState::Failed, Some("j"), graph::Code::Data));
    assert!(
        r.problems[0].message.contains("node owners is not unique on its keys") && r.problems[0].message.contains("'NY'"),
        "{:?}",
        r.problems
    );
    // A key it names that a side does not have.
    let e = run(
        &prep,
        &ann,
        &d.id,
        graph(json!([
            { "id": "sales", "kind": "source", "source": sales },
            { "id": "owners", "kind": "source", "source": owners.id },
            { "id": "j", "kind": "join", "left": "sales", "right": "owners", "on": [{ "left": "region", "right": "areas" }] },
            { "id": "out", "kind": "output", "input": "j" },
        ])),
    )
    .err()
    .unwrap();
    assert!(e.contains("node j: the join reads column 'areas' of its right side, owners, which is not there"), "{e}");
    // A dataset of another project is not there to be read.
    let (_, theirs) = upload(&prep, &Scope { project: "other".into(), ..scope() }, "area,manager\nNY,x\n");
    let e = run(&prep, &ann, &d.id, joined(json!({ "id": "owners", "kind": "dataset", "dataset": theirs.id }), "left")).err().unwrap();
    assert!(e.contains("it reads dataset") && e.contains("not in this project"), "{e}");
}

#[test]
fn an_append_node_meets_columns_by_name() {
    let (_dir, prep) = prep();
    let ann = scope();
    let jan = source(&prep, &ann, "region,amount\nNY,10\n");
    let feb = source(&prep, &ann, "amount,region,note\n2.5,CA,late\n");
    let mar = source(&prep, &ann, "region\nTX\n");
    let d = prep.create_dataset(&ann, Named::default(), None).unwrap();
    let r = ran(
        &prep,
        &ann,
        &d.id,
        graph(json!([
            { "id": "jan", "kind": "source", "source": jan },
            { "id": "feb", "kind": "source", "source": feb },
            { "id": "mar", "kind": "source", "source": mar },
            { "id": "all", "kind": "append", "inputs": ["jan", "feb", "mar"] },
            { "id": "out", "kind": "output", "input": "all" },
        ])),
    );
    let cols: Vec<(String, ColumnType)> = columns_of(&prep, &ann, &d.id).into_iter().map(|c| (c.name, c.ty)).collect();
    assert_eq!(cols, [("region".into(), Text), ("amount".into(), Decimal(1)), ("note".into(), Text)]);
    assert_eq!(rows_of(&prep, &ann, &d.id), [json!(["NY", "10.0", null]), json!(["CA", "2.5", "late"]), json!(["TX", null, null])]);
    let all = r.nodes.iter().find(|n| n.id == "all").unwrap();
    assert_eq!(all.summary, "Appended 3 inputs: 1 + 1 + 1 rows, 3 in all; 2 column(s) only in some, empty in the rest.");
}

#[test]
fn a_sql_node_reads_its_inputs_by_the_aliases_it_gives_them() {
    let (_dir, prep) = prep();
    let ann = scope();
    let sales = source(&prep, &ann, SALES);
    let stores = source(&prep, &ann, "store,city\na,Albany\nb,Fresno\nc,Ithaca\n");
    let d = prep.create_dataset(&ann, Named::default(), None).unwrap();
    let g = |q: &str| {
        graph(json!([
            { "id": "s", "kind": "source", "source": sales },
            { "id": "t", "kind": "source", "source": stores },
            { "id": "q", "kind": "sql", "inputs": { "sales": "s", "stores": "t" }, "query": q },
            { "id": "out", "kind": "output", "input": "q" },
        ]))
    };
    let r =
        ran(&prep, &ann, &d.id, g("SELECT city, sum(amount) AS total FROM sales JOIN stores USING (store) GROUP BY city ORDER BY city"));
    assert_eq!(rows_of(&prep, &ann, &d.id), [json!(["Albany", 10]), json!(["Fresno", 5]), json!(["Ithaca", 7])]);
    assert_eq!(r.nodes[2].summary, "Ran a query: 6 rows in, 3 rows, 2 columns out.");
    let e = run(&prep, &ann, &d.id, g("SELECT * FROM s")).err().unwrap();
    assert!(e.contains("node q: it reads s; it reads sales, stores"), "{e}");
    let e = run(&prep, &ann, &d.id, g("SELECT nope FROM sales")).err().unwrap();
    assert!(e.contains("node q: its query reads column 'nope'"), "{e}");
}

#[test]
fn a_graph_reads_a_datasets_output_and_a_version_of_it() {
    let (_dir, prep) = prep();
    let ann = scope();
    let (s, base) = upload(&prep, &ann, SALES);
    prep.add_upload(&ann, &s.id, csv("region,store,amount\nNY,a,1\n")).unwrap();
    ran(&prep, &ann, &base.id, prep.version(&ann, &base.id, None).unwrap().graph.unwrap());
    let d = prep.create_dataset(&ann, Named::default(), None).unwrap();
    let reading = |read: Value| {
        graph(json!([{ "id": "b", "kind": "dataset", "dataset": base.id, "read": read }, { "id": "out", "kind": "output", "input": "b" }]))
    };
    ran(&prep, &ann, &d.id, reading(json!("latest")));
    assert_eq!(prep.get(&ann, &d.id).unwrap().rows, 1);
    let r = ran(&prep, &ann, &d.id, reading(json!({ "version": 1 })));
    assert_eq!((prep.get(&ann, &d.id).unwrap().rows, r.inputs[0].number), (3, Some(1)));
    assert!(run(&prep, &ann, &d.id, reading(json!("pull"))).err().unwrap().contains("it is not pulled"));
    assert!(run(&prep, &ann, &d.id, reading(json!({ "snapshot": 1 }))).err().unwrap().contains("versions, not snapshots"));
}

// ------------------------------------------------------------- edits

#[test]
fn an_edits_node_applies_edits_by_key_and_says_how_each_fared() {
    let (_dir, prep) = prep();
    let ann = scope();
    let src = source(&prep, &ann, "store,region,amount\n1,NY,10\n2,CA,5\n3,NY,7\n4,TX,1\n");
    let d = prep.create_dataset(&ann, Named::default(), None).unwrap();
    let with = |edits: Value| {
        graph(json!([
            { "id": "in", "kind": "source", "source": src },
            { "id": "fix", "kind": "edits", "input": "in", "key": ["store"], "edits": edits },
            { "id": "big", "kind": "steps", "input": "fix", "steps": [{ "op": "filter", "formula": "amount > 2" }] },
            { "id": "out", "kind": "output", "input": "big" },
        ]))
    };
    let edits = json!([
        { "id": "e1", "kind": "remap", "column": "region", "from": "NY", "to": "New York" },
        { "id": "e2", "kind": "set", "column": "amount", "key": { "store": 2 }, "to": 50, "was": 5 },
        { "id": "e3", "kind": "delete", "key": { "store": 4 } },
        { "id": "e4", "kind": "add", "key": { "store": 9 }, "values": { "region": "WA", "amount": 3 } },
        { "id": "e5", "kind": "set", "column": "amount", "key": { "store": 77 }, "to": 1 },
    ]);
    let r = ran(&prep, &ann, &d.id, with(edits.clone()));
    assert_eq!(
        rows_of(&prep, &ann, &d.id),
        [json!([1, "New York", 10]), json!([2, "CA", 50]), json!([3, "New York", 7]), json!([9, "WA", 3])],
        "applied where the node is: before the filter, which then keeps the row edited up"
    );
    let fix = r.nodes.iter().find(|n| n.id == "fix").unwrap();
    let states: Vec<(&str, State)> = fix.edits.iter().map(|o| (o.id.as_str(), o.state.clone())).collect();
    assert!(states.contains(&("e5", State::Orphaned)) && states.contains(&("e2", State::Applied)), "{states:?}");
    assert_eq!(fix.summary, "5 edits: 4 applied, 1 found no row.");
    assert!(prep.get(&ann, &d.id).unwrap().notes.iter().any(|n| n.contains("e5")), "an orphan is a note");
    // Upstream moves the value an edit replaced: the user's value wins, flagged.
    prep.add_upload(&ann, &src, csv("store,region,amount\n1,NY,10\n2,CA,6\n")).unwrap();
    let r = ran(&prep, &ann, &d.id, with(edits));
    assert_eq!(rows_of(&prep, &ann, &d.id)[1], json!([2, "CA", 50]));
    let fix = r.nodes.iter().find(|n| n.id == "fix").unwrap();
    assert_eq!(fix.edits.iter().find(|o| o.id == "e2").unwrap().state, State::Flagged);
    // A key that is not unique where the node is fails the run.
    prep.add_upload(&ann, &src, csv("store,region,amount\n1,NY,10\n1,CA,6\n")).unwrap();
    let r = run(&prep, &ann, &d.id, with(json!([]))).unwrap();
    assert!(r.problems[0].message.contains("its key store is not unique here"), "{:?}", r.problems);
    // Edits that do not fit the node are refused before anything runs.
    for (edits, says) in [
        (
            json!([{ "id": "x", "kind": "set", "column": "amount", "key": { "region": "NY" }, "to": 1 }]),
            "its key names region; the node's key is store",
        ),
        (json!([{ "id": "x", "kind": "remap", "column": "gone", "from": 1, "to": 2 }]), "edit x reads column 'gone'"),
        (json!([{ "id": "x", "kind": "set", "column": "amount", "key": { "store": 1 }, "to": "lots" }]), "not a value of column 'amount'"),
        (
            json!([{ "id": "x", "kind": "delete", "key": { "store": 1 } }, { "id": "x", "kind": "delete", "key": { "store": 2 } }]),
            "two edits have the id 'x'",
        ),
    ] {
        let e = run(&prep, &ann, &d.id, with(edits)).err().unwrap();
        assert!(e.contains("node fix") && e.contains(says), "{says}: {e}");
    }
}

// ----------------------------------------------------- output contracts

#[test]
fn an_output_check_that_holds_keeps_nothing_and_one_that_warns_lets_it_through() {
    let (_dir, prep) = prep();
    let ann = scope();
    let src = source(&prep, &ann, "store,amount\n1,10\n2,5\n");
    let d = prep.create_dataset(&ann, Named::default(), None).unwrap();
    let out = |extra: Value| {
        let mut o = json!({ "id": "out", "kind": "output", "input": "in", "key": ["store"], "roles": { "store": "dimension" } });
        o.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
        graph(json!([{ "id": "in", "kind": "source", "source": src }, o]))
    };
    let r = ran(&prep, &ann, &d.id, out(json!({})));
    assert_eq!(r.nodes[1].summary, "Output: 2 rows, 2 columns; 2 checks passed.");
    assert_eq!(columns_of(&prep, &ann, &d.id)[0].role, Role::Dimension, "the output gives the roles");
    prep.add_upload(&ann, &src, csv("store,amount\n1,10\n1,5\n,3\n")).unwrap();
    let r = run(&prep, &ann, &d.id, out(json!({}))).unwrap();
    assert_eq!(r.state, RunState::Held);
    assert_eq!(r.summary, "Held: store unique, store never empty failed; version 1 is still live.");
    assert_eq!(r.nodes[1].checks.iter().map(|c| c.failing).collect::<Vec<_>>(), [1, 1]);
    assert_eq!(prep.get(&ann, &d.id).unwrap().live, Some(1));
    let warn = out(json!({ "key": [], "checks": [{ "check": "not_null", "columns": ["store"], "on_fail": "warn" }] }));
    let r = ran(&prep, &ann, &d.id, warn);
    assert!(r.summary.contains("check failed, let through: store never empty (1 failing)"), "{}", r.summary);
    assert_eq!(prep.get(&ann, &d.id).unwrap().live, Some(2));
    let e = run(&prep, &ann, &d.id, out(json!({ "checks": [{ "check": "unique", "columns": ["nope"] }] }))).err().unwrap();
    assert!(e.contains("node out: check 1 reads column 'nope'"), "{e}");
}

// ------------------------------------------------- query sources, runs

fn arrived(regions: &[&str], revenue: &[i128], context: &str) -> Arrived {
    use arrow_array::{ArrayRef, Decimal128Array, Int32Array, RecordBatch, StringArray};
    use arrow_schema::{DataType, Field, Schema};
    let schema = std::sync::Arc::new(Schema::new(vec![
        Field::new("region", DataType::Utf8, true),
        Field::new("year", DataType::Int32, true),
        Field::new("revenue", DataType::Decimal128(10, 2), true),
    ]));
    let n = regions.len();
    let arrays: Vec<ArrayRef> = vec![
        std::sync::Arc::new(StringArray::from(regions.to_vec())),
        std::sync::Arc::new(Int32Array::from(vec![2026; n])),
        std::sync::Arc::new(Decimal128Array::from(revenue.to_vec()).with_precision_and_scale(10, 2).unwrap()),
    ];
    let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
    Arrived {
        rows: Batches { schema, batches: vec![batch] },
        roles: [("region", Role::Dimension), ("year", Role::Dimension), ("revenue", Role::Measure)]
            .into_iter()
            .map(|(n, r)| (n.to_string(), r))
            .collect(),
        read: SemanticRead {
            branch: "main".into(),
            deployed_at: "2026-11-26T10:00Z".into(),
            spec: Some(json!({ "fields": ["region", "year", "revenue"] })),
            expr: None,
            sql_sha256: "ab".repeat(32),
            context: context.into(),
            fields: [("revenue".to_string(), "orders.revenue".to_string())].into_iter().collect(),
        },
    }
}

/// A run whose pulls the test answers, as the server would, with rows
/// taken as `context`.
fn run_pulling(prep: &Prep, scope: &Scope, ds: &str, g: Graph, context: &str, rows: &[&str]) -> (Vec<String>, RunRecord) {
    let request = RunRequest { id: run_id(), dataset: ds.into(), graph: g };
    let Begun::Started(mut t) = prep.begin(scope, request, context).unwrap() else { panic!("a new run id") };
    let pulled: Vec<String> = t.pulls.iter().map(|p| p.source.clone()).collect();
    for p in pulled.clone() {
        prep.pulled_for(&mut t, &p, arrived(rows, &vec![100; rows.len()], context)).unwrap();
    }
    (pulled, prep.execute(*t))
}

#[test]
fn a_query_source_is_pulled_by_a_run_as_its_context_and_read_only_as_it() {
    let (_dir, prep) = prep();
    let ann = scope();
    let query = Query { branch: "main".into(), spec: Some(json!({ "fields": ["region", "revenue"] })), expr: None };
    let s = prep.create_query(&ann, Named { name: Some("revenue".into()), description: None }, query.clone()).unwrap();
    assert_eq!((s.kind, s.snapshot, s.query.clone()), (Kind::Query, 0, Some(query)));
    let d = prep.create_dataset(&ann, Named::default(), None).unwrap();
    let g = |read: Value| {
        graph(json!([{ "id": "rev", "kind": "source", "source": s.id, "read": read }, { "id": "out", "kind": "output", "input": "rev" }]))
    };
    // Never pulled: the dry run says so, and a run pulls it.
    let c = prep.check(&ann, &g(json!("latest")), "ann").unwrap();
    assert!(c.ok && c.said.starts_with("Pulls revenue (it has never been pulled)"), "{}", c.said);
    let (pulled, r) = run_pulling(&prep, &ann, &d.id, g(json!("latest")), "ann", &["NY", "CA"]);
    assert_eq!((pulled, r.state), (vec![s.id.clone()], RunState::Succeeded), "{}", r.summary);
    assert_eq!(r.inputs[0].will, record::Will::Pull);
    let kinds: Vec<(String, ColumnType, Role)> = columns_of(&prep, &ann, &d.id).into_iter().map(|c| (c.name, c.ty, c.role)).collect();
    assert_eq!(
        kinds,
        [
            ("region".into(), Text, Role::Dimension),
            ("year".into(), Integer, Role::Dimension),
            ("revenue".into(), Decimal(2), Role::Measure)
        ]
    );
    let read = prep.lineage(&ann, &d.id, None).unwrap().sources[0].semantic.clone().unwrap();
    assert_eq!((read.context.as_str(), read.fields["revenue"].as_str()), ("ann", "orders.revenue"));
    // Latest, as the same context: the snapshot it took, no pull.
    let (pulled, _) = run_pulling(&prep, &ann, &d.id, g(json!("latest")), "ann", &[]);
    assert!(pulled.is_empty());
    // Another context reads its own: a pull, never ann's rows.
    let (pulled, _) = run_pulling(&prep, &ann, &d.id, g(json!("latest")), "bo", &["TX"]);
    assert_eq!(pulled.len(), 1);
    assert_eq!(rows_of(&prep, &ann, &d.id), [json!(["TX", 2026, "1.00"])]);
    // Pull, and a max age a fresh snapshot is inside.
    assert_eq!(run_pulling(&prep, &ann, &d.id, g(json!("pull")), "ann", &["NY"]).0.len(), 1);
    assert!(run_pulling(&prep, &ann, &d.id, g(json!({ "max_age": 3600 })), "ann", &[]).0.is_empty());
    // A snapshot named that another context took is refused.
    let e = prep.begin(&ann, RunRequest { id: run_id(), dataset: d.id.clone(), graph: g(json!({ "snapshot": 2 })) }, "ann").err().unwrap();
    assert!(e.to_string().contains("snapshot 2 of revenue was taken as another context"), "{e}");
    // A query source takes no file.
    assert!(prep.add_upload(&ann, &s.id, csv(BUDGET)).err().unwrap().to_string().contains("pulled by a run"));
}

#[test]
fn a_run_id_answers_its_first_request_and_a_dataset_takes_one_run_at_a_time() {
    let (_dir, prep) = prep();
    let ann = scope();
    let (s, d) = upload(&prep, &ann, SALES);
    let g = steps_graph(&s.id, json!([]));
    let ask = |id: &str, g: Graph| prep.begin(&ann, RunRequest { id: id.into(), dataset: d.id.clone(), graph: g }, ANN);
    let Begun::Started(t) = ask("r1", g.clone()).unwrap() else { panic!() };
    assert!(matches!(ask("r2", g.clone()), Err(Error::Conflict(m)) if m.contains("being made by run r1")));
    let Begun::Answered(running) = ask("r1", g.clone()).unwrap() else { panic!("a retry") };
    assert_eq!(running.state, RunState::Running);
    let done = prep.execute(*t);
    let Begun::Answered(again) = ask("r1", g.clone()).unwrap() else { panic!() };
    assert_eq!((again.state, again.version), (RunState::Succeeded, done.version));
    assert_eq!(prep.run(&ann, "r1").unwrap().version, Some(2));
    let other = steps_graph(&s.id, json!([{ "op": "sort", "by": [{ "column": "amount" }] }]));
    assert!(matches!(ask("r1", other), Err(Error::Conflict(m)) if m.contains("a new run takes a new id")));
    assert!(matches!(ask("../r", g), Err(Error::Invalid(_))));
    assert_eq!(prep.get(&ann, &d.id).unwrap().last_run.unwrap().id, "r1");
}

#[test]
fn a_dry_run_says_what_a_run_would_read_and_each_nodes_columns() {
    let (_dir, prep) = prep();
    let ann = scope();
    let src = source(&prep, &ann, SALES);
    let c = prep.check(&ann, &steps_graph(&src, json!([{ "op": "remove_columns", "columns": ["store"] }])), ANN).unwrap();
    assert!(c.ok);
    assert_eq!(c.said, "Reads budget (snapshot 1, its newest), runs 3 nodes.");
    let shape = c.nodes.iter().find(|n| n.id == "shape").unwrap();
    assert_eq!(shape.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["region", "amount"]);
    let c = prep.check(&ann, &graph(json!([{ "id": "a", "kind": "source", "source": src }])), ANN).unwrap();
    assert!(!c.ok && c.problems[0].message.contains("has none"), "{:?}", c.problems);
    assert!(prep.list(&ann).is_empty(), "a dry run makes nothing");
}

#[test]
fn a_sources_types_are_how_it_is_read_and_a_node_reads_with_its_own() {
    let (_dir, prep) = prep();
    let ann = scope();
    let src = source(&prep, &ann, "store,code\n1,2134\n2,10001\n");
    assert_eq!(prep.source(&ann, &src).unwrap().columns[1].ty, Integer, "a code looks like a number");
    let d = prep.create_dataset(&ann, Named::default(), None).unwrap();
    let types: BTreeMap<String, ColumnType> = [("code".to_string(), Text)].into_iter().collect();
    let s = prep.change_source(&ann, &src, SourceChange { types: Some(types), ..Default::default() }).unwrap();
    assert_eq!((s.read, s.reads.len()), (2, 2));
    let r = ran(&prep, &ann, &d.id, steps_graph(&src, json!([])));
    assert_eq!(columns_of(&prep, &ann, &d.id)[1].ty, Text);
    assert!(r.nodes[0].summary.contains("read code as text"), "{}", r.nodes[0].summary);
    assert_eq!(prep.lineage(&ann, &d.id, None).unwrap().sources[0].read, 2);
    // A node reads with its own, for itself only.
    let g = graph(json!([
        { "id": "in", "kind": "source", "source": src, "types": { "store": "text" } },
        { "id": "out", "kind": "output", "input": "in" },
    ]));
    ran(&prep, &ann, &d.id, g);
    assert_eq!(columns_of(&prep, &ann, &d.id).iter().map(|c| c.ty).collect::<Vec<_>>(), [Text, Text]);
    assert!(prep.create_dataset(&ann, Named::default(), Some(&src)).is_err(), "read with types, it is not a dataset as it is");
    let g = graph(
        json!([{ "id": "in", "kind": "source", "source": src, "types": { "nope": "text" } }, { "id": "out", "kind": "output", "input": "in" }]),
    );
    assert!(run(&prep, &ann, &d.id, g).err().unwrap().contains("the types it is read with reads column 'nope'"));
}

#[test]
fn a_source_keeps_its_newest_snapshots_and_every_one_an_output_needs() {
    let (_dir, prep) = prep();
    let ann = scope();
    let (s, d) = upload(&prep, &ann, "a\n1\n");
    for i in 2..=23 {
        prep.add_upload(&ann, &s.id, csv(&format!("a\n{i}\n"))).unwrap();
    }
    let kept: Vec<u32> = prep.snapshots(&ann, &s.id).unwrap().iter().map(|x| x.number).collect();
    assert_eq!(kept.len(), 21, "the newest 20 and the one the dataset is: {kept:?}");
    assert!(kept.contains(&1) && !kept.contains(&2) && kept.contains(&23));
    assert_eq!(rows_of(&prep, &ann, &d.id), [json!([1])]);
}

// ---------------------------------------------------------------- blends

mod blends {
    use super::*;
    use crate::blend::{Attach, Blend, Join, Kind, ModelPart, On, Output};
    use arrow_array::{ArrayRef, Decimal128Array, RecordBatch, StringArray};
    use arrow_schema::{DataType, Field, Schema};
    use std::sync::Arc;

    /// Revenue by region and month, as the warehouse would answer it.
    fn model(rows: &[(&str, &str, i128)]) -> ModelPart {
        let schema = Arc::new(Schema::new(vec![
            Field::new("__m0", DataType::Utf8, true),
            Field::new("__m1", DataType::Utf8, true),
            Field::new("__m2", DataType::Decimal128(12, 2), true),
        ]));
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(rows.iter().map(|r| r.0).collect::<Vec<_>>())),
            Arc::new(StringArray::from(rows.iter().map(|r| r.1).collect::<Vec<_>>())),
            Arc::new(Decimal128Array::from(rows.iter().map(|r| r.2).collect::<Vec<_>>()).with_precision_and_scale(12, 2).unwrap()),
        ];
        ModelPart {
            rows: Batches { schema: schema.clone(), batches: vec![RecordBatch::try_new(schema, arrays).unwrap()] },
            keys: [("store.region", "__m0"), ("date.month", "__m1")].into_iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
        }
    }

    fn out_model(column: &str, name: &str, measure: bool) -> Output {
        Output::Model { column: column.into(), name: name.into(), measure }
    }

    fn out_ds(alias: &str, column: &str) -> Output {
        Output::Dataset { alias: alias.into(), column: column.into(), name: format!("{alias}.{column}") }
    }

    fn on(l: &str, r: &str) -> On {
        On { left: l.into(), right: r.into() }
    }

    fn rows(b: &crate::Blended) -> Vec<Value> {
        zsql_exec::json::rows(&b.rows).unwrap()
    }

    struct Fixture {
        _dir: Dir,
        prep: Prep,
        ann: Scope,
        budget: String,
        budget_source: String,
        owners: String,
        targets: String,
    }

    fn fixture() -> Fixture {
        let (dir, prep) = prep();
        let ann = scope();
        let (bs, budget) = upload(&prep, &ann, "region,month,amount\nNY,2026-01,100\nNY,2026-02,90\nCA,2026-01,50\n");
        let owners = upload(&prep, &ann, "region,manager\nNY,kim\nCA,lee\n").1.id;
        let targets = upload(&prep, &ann, "region\nNY\nNY\n").1.id;
        Fixture { _dir: dir, prep, ann, budget: budget.id, budget_source: bs.id, owners, targets }
    }

    #[test]
    fn budget_against_actual_keeps_rows_from_both_sides() {
        let f = fixture();
        let m = model(&[("NY", "2026-01", 12000), ("CA", "2026-03", 500)]);
        let plan = Blend {
            datasets: vec![
                Attach { id: f.budget.clone(), alias: "budget".into() },
                Attach { id: f.owners.clone(), alias: "owners".into() },
            ],
            joins: vec![
                Join {
                    join: "budget".into(),
                    to: "model".into(),
                    on: vec![on("store.region", "budget.region"), on("date.month", "budget.month")],
                    kind: Kind::Enrich,
                },
                Join { join: "owners".into(), to: "budget".into(), on: vec![on("budget.region", "owners.region")], kind: Kind::Enrich },
            ],
            outputs: vec![
                out_model("__m0", "region", false),
                out_model("__m1", "month", false),
                out_model("__m2", "revenue", true),
                out_ds("budget", "amount"),
                out_ds("owners", "manager"),
            ],
        };
        let b = f.prep.blend(&f.ann, &plan, &m, "select 1").unwrap();
        let mut got = rows(&b);
        got.sort_by_key(|r| r.to_string());
        assert_eq!(
            got,
            [
                json!(["CA", "2026-01", null, 50, "lee"]),
                json!(["CA", "2026-03", "5.00", null, "lee"]),
                json!(["NY", "2026-01", "120.00", 100, "kim"]),
                json!(["NY", "2026-02", null, 90, "kim"]),
            ],
            "a month with a budget and no sales still shows"
        );
        assert!(!b.hit);
        assert_eq!(b.datasets.len(), 2);
        assert!(f.prep.blend(&f.ann, &plan, &m, "select 1").unwrap().hit, "the same question over the same data is kept");
        assert!(!f.prep.blend(&f.ann, &plan, &m, "select 2").unwrap().hit, "another run of the model's part is another question");
        // A new version of a dataset is another question too.
        f.prep.add_upload(&f.ann, &f.budget_source, csv("region,month,amount\nNY,2026-01,1\n")).unwrap();
        ran(&f.prep, &f.ann, &f.budget, steps_graph(&f.budget_source, json!([])));
        assert!(!f.prep.blend(&f.ann, &plan, &m, "select 1").unwrap().hit);
    }

    #[test]
    fn a_join_key_not_asked_for_is_refused() {
        let f = fixture();
        let m = model(&[("NY", "2026-01", 100), ("NY", "2026-02", 200)]);
        let plan = Blend {
            datasets: vec![Attach { id: f.budget.clone(), alias: "budget".into() }],
            joins: vec![Join {
                join: "budget".into(),
                to: "model".into(),
                on: vec![on("store.region", "budget.region"), on("date.month", "budget.month")],
                kind: Kind::Enrich,
            }],
            outputs: vec![out_model("__m0", "region", false), out_model("__m2", "revenue", true), out_ds("budget", "amount")],
        };
        let e = f.prep.blend(&f.ann, &plan, &m, "q").err().unwrap();
        assert!(e.to_string().contains("'date.month' joins a dataset to the model, so ask for it too"), "{e}");
    }

    #[test]
    fn a_join_on_no_keys_is_a_cross_join() {
        let f = fixture();
        let target = upload(&f.prep, &f.ann, "target\n150\n").1.id;
        let m = model(&[("NY", "2026-01", 12000), ("CA", "2026-01", 500)]);
        let mut plan = Blend {
            datasets: vec![Attach { id: target, alias: "goal".into() }],
            joins: vec![Join { join: "goal".into(), to: "model".into(), on: vec![], kind: Kind::Enrich }],
            outputs: vec![out_model("__m0", "region", false), out_model("__m2", "revenue", true), out_ds("goal", "target")],
        };
        assert_eq!(rows(&f.prep.blend(&f.ann, &plan, &m, "q").unwrap()), [json!(["NY", "120.00", 150]), json!(["CA", "5.00", 150])]);
        plan.joins[0].kind = Kind::Filter;
        plan.outputs.pop();
        assert!(f.prep.blend(&f.ann, &plan, &m, "q2").err().unwrap().to_string().contains("a filter on nothing"));
    }

    #[test]
    fn a_dimension_the_dataset_does_not_have_is_refused() {
        let f = fixture();
        let m = model(&[("NY", "2026-01", 100)]);
        let plan = Blend {
            datasets: vec![Attach { id: f.budget.clone(), alias: "budget".into() }],
            joins: vec![Join {
                join: "budget".into(),
                to: "model".into(),
                on: vec![on("store.region", "budget.region")],
                kind: Kind::Enrich,
            }],
            outputs: vec![out_model("__m0", "region", false), out_model("__m1", "month", false), out_ds("budget", "amount")],
        };
        let e = f.prep.blend(&f.ann, &plan, &m, "q").err().unwrap();
        assert!(e.to_string().contains("'month' is asked for") && e.to_string().contains("would repeat"), "{e}");
    }

    #[test]
    fn a_filter_keeps_what_the_dataset_has_and_a_lookup_must_be_unique() {
        let f = fixture();
        let m = model(&[("NY", "2026-01", 100), ("CA", "2026-01", 50)]);
        let plan = Blend {
            datasets: vec![Attach { id: f.targets.clone(), alias: "targets".into() }],
            joins: vec![Join {
                join: "targets".into(),
                to: "model".into(),
                on: vec![on("store.region", "targets.region")],
                kind: Kind::Filter,
            }],
            outputs: vec![out_model("__m0", "region", false), out_model("__m2", "revenue", true)],
        };
        assert_eq!(rows(&f.prep.blend(&f.ann, &plan, &m, "q").unwrap()), [json!(["NY", "1.00"])], "a semi-join: no row repeats");
        let mut lookup = plan;
        lookup.joins[0].kind = Kind::Enrich;
        lookup.outputs.push(out_ds("targets", "region"));
        assert!(f.prep.blend(&f.ann, &lookup, &m, "q").err().unwrap().to_string().contains("not unique on its keys"));
    }

    #[test]
    fn every_dataset_is_joined_once_and_only_as_the_request_says() {
        let f = fixture();
        let m = model(&[("NY", "2026-01", 100)]);
        let attach = vec![Attach { id: f.owners.clone(), alias: "owners".into() }];
        let join =
            |to: &str| Join { join: "owners".into(), to: to.into(), on: vec![on("store.region", "owners.region")], kind: Kind::Enrich };
        let outputs = vec![out_model("__m0", "region", false)];
        let cases: Vec<(Blend, &str)> = vec![
            (Blend { datasets: attach.clone(), joins: vec![], outputs: outputs.clone() }, "joined to nothing"),
            (Blend { datasets: attach.clone(), joins: vec![join("model"), join("model")], outputs: outputs.clone() }, "joined 2 times"),
            (Blend { datasets: attach.clone(), joins: vec![join("owners")], outputs: outputs.clone() }, "do not connect"),
            (
                Blend { datasets: attach.clone(), joins: vec![join("elsewhere")], outputs: outputs.clone() },
                "neither the model nor attached",
            ),
            (
                Blend { datasets: vec![Attach { id: f.owners.clone(), alias: "model".into() }], joins: vec![], outputs: outputs.clone() },
                "not an alias",
            ),
        ];
        for (plan, says) in cases {
            let e = f.prep.blend(&f.ann, &plan, &m, "q").err().unwrap();
            assert!(e.to_string().contains(says), "{says}: {e}");
        }
        let plan = Blend { datasets: attach, joins: vec![join("model")], outputs };
        assert!(matches!(f.prep.blend(&Scope { project: "other".into(), ..scope() }, &plan, &m, "q"), Err(Error::NotFound(_))));
    }
}

// ---------------------------------------------------------------- backup

#[test]
fn every_source_and_dataset_is_copied_to_the_object_store_and_restored_from_it() {
    let (dir, _) = prep();
    let store_dir = dir.0.join("store");
    std::fs::create_dir_all(&store_dir).unwrap();
    let objects = std::sync::Arc::new(zsql_exec::Objects::open(&format!("file://{}", store_dir.display()), &[]).unwrap());
    let data = dir.0.join("data");
    let open = || reopen(&data).with_backup(Some(objects.clone()));
    let prep = open();
    let ann = scope();
    let (s, d) = upload(&prep, &ann, BUDGET);
    assert_eq!(prep.get(&ann, &d.id).unwrap().backup.map(|b| b.failed), Some(None));
    assert_eq!(prep.source(&ann, &s.id).unwrap().backup.map(|b| b.failed), Some(None));
    let fix = graph(json!([
        { "id": "in", "kind": "source", "source": s.id },
        { "id": "fix", "kind": "edits", "input": "in", "edits": [{ "id": "e1", "kind": "remap", "column": "region", "from": "EMEA", "to": "Europe" }] },
        { "id": "out", "kind": "output", "input": "fix" },
    ]));
    ran(&prep, &ann, &d.id, fix);
    let copied: Vec<String> = objects.list(&["datasets", "default", "shop", &d.id]).unwrap().into_iter().map(|k| k.join("/")).collect();
    for f in ["dataset.json", "v1/manifest.json", "v2/rows.arrow", "v2/lineage.json", "v2/graph.json", "v2/run.json"] {
        assert!(copied.contains(&f.to_string()), "{f} not in {copied:?}");
    }
    let sent: Vec<String> =
        objects.list(&["datasets", "default", "shop", "sources", &s.id]).unwrap().into_iter().map(|k| k.join("/")).collect();
    assert!(sent.contains(&"source.json".to_string()) && sent.contains(&"snapshots/1/rows.arrow".to_string()), "{sent:?}");

    // The disk is lost; a restore brings both back whole, the source first.
    std::fs::remove_dir_all(data.join("default")).unwrap();
    let prep = open();
    assert!(prep.get(&ann, &d.id).is_err());
    assert_eq!(prep.restore("default").unwrap(), [format!("shop/{}", s.id), format!("shop/{}", d.id)]);
    assert_eq!(rows_of(&prep, &ann, &d.id)[0][0], "Europe");
    assert_eq!(
        json_rows(&prep.page(&ann, &d.id, Some(1), 0, 10, &View::default()).unwrap())[0][0],
        "EMEA",
        "the output that is the snapshot"
    );
    assert!(prep.restore("default").unwrap().is_empty(), "what is here is not brought back again");
    prep.delete(&ann, &d.id).unwrap();
    assert!(objects.list(&["datasets", "default", "shop", &d.id]).unwrap().is_empty());
    prep.delete_source(&ann, &s.id).unwrap();
    assert!(objects.list(&["datasets", "default", "shop", "sources", &s.id]).unwrap().is_empty());
    let (_d2, plain) = self::prep();
    assert!(matches!(plain.restore("default"), Err(Error::Invalid(_))));
}

// -------------------------------------------------------------- recovery

mod recovery_tests {
    use super::*;
    use crate::recovery::{Cause, JobKind};

    #[test]
    fn a_run_found_running_at_start_is_interrupted_and_never_run_again() {
        let (dir, prep) = prep();
        let ann = scope();
        let (s, d) = upload(&prep, &ann, BUDGET);
        let Begun::Started(_t) = prep
            .begin(&ann, RunRequest { id: "crashed".into(), dataset: d.id.clone(), graph: steps_graph(&s.id, json!([])) }, ANN)
            .unwrap()
        else {
            panic!()
        };
        // The process dies with it running.
        drop(prep);
        let prep = reopen(&dir.0);
        let r = prep.run(&ann, "crashed").unwrap();
        assert_eq!(r.state, RunState::Interrupted);
        let i = r.interrupted.unwrap();
        assert_eq!((i.job, i.cause), (JobKind::Run, Cause::Crash));
        assert!(
            i.reason.contains("may be what stopped the server, so it was not run again") && i.reason.contains("Send it again"),
            "{}",
            i.reason
        );
        let got = prep.get(&ann, &d.id).unwrap();
        assert_eq!((got.live, got.last_run.map(|l| l.state)), (Some(1), Some(RunState::Interrupted)), "it keeps the output it had");
        assert!(got.interrupted.is_some());
        // The next run the application sends ends it.
        ran(&prep, &ann, &d.id, steps_graph(&s.id, json!([])));
        assert!(prep.get(&ann, &d.id).unwrap().interrupted.is_none());
        // So does the same id: it answers what it came to.
        let Begun::Answered(again) = prep
            .begin(&ann, RunRequest { id: "crashed".into(), dataset: d.id.clone(), graph: steps_graph(&s.id, json!([])) }, ANN)
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(again.state, RunState::Interrupted);
    }

    #[test]
    fn a_stop_suspends_what_runs_past_its_grace_and_the_next_start_says_so() {
        let (dir, prep) = prep();
        let ann = scope();
        let (s, d) = upload(&prep, &ann, BUDGET);
        let slow = json!([{ "op": "sql", "query": "SELECT region, month, amount FROM input, range(100000000000) a, range(1000) b" }]);
        let Begun::Started(t) =
            prep.begin(&ann, RunRequest { id: "slow".into(), dataset: d.id.clone(), graph: steps_graph(&s.id, slow) }, ANN).unwrap()
        else {
            panic!()
        };
        let started = std::time::Instant::now();
        std::thread::scope(|scope| {
            let job = scope.spawn(|| prep.execute(*t));
            std::thread::sleep(std::time::Duration::from_millis(800));
            prep.stop(std::time::Duration::from_millis(200));
            assert_eq!(job.join().unwrap().state, RunState::Suspended);
        });
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        let e = prep.begin(&ann, RunRequest { id: "late".into(), dataset: d.id.clone(), graph: steps_graph(&s.id, json!([])) }, ANN);
        assert!(matches!(e, Err(Error::Stopping(_))), "nothing new starts");
        assert!(matches!(prep.add_upload(&ann, &s.id, csv(BUDGET)), Err(Error::Stopping(_))));
        drop(prep);
        let prep = reopen(&dir.0);
        let i = prep.run(&ann, "slow").unwrap().interrupted.unwrap();
        assert_eq!((i.job, i.cause), (JobKind::Run, Cause::Stop));
        assert!(i.reason.contains("stopped while this run ran, and nothing is resumed"), "{}", i.reason);
    }

    #[test]
    fn a_snapshot_cut_short_marks_its_source_and_what_it_left_is_swept() {
        let (dir, prep) = prep();
        let ann = scope();
        let (s, d) = upload(&prep, &ann, BUDGET);
        let root = dir.0.join("default/shop").join(DIR);
        let sdir = root.join(store::SOURCES).join(&s.id);
        let marker = json!({ "job": "upload", "started": "2026-11-27T00:00:00.000Z", "state": "running" });
        std::fs::write(sdir.join(recovery::MARKER), marker.to_string()).unwrap();
        let left = [
            dir.0.join(".cache/prep/s999-0-1"),
            root.join(".new-ds_0000000000000000"),
            root.join(store::SOURCES).join(".new-src_0000000000000000"),
            root.join(".deleting-ds_1111111111111111-5"),
            root.join(&d.id).join(".v9.staging-123"),
            sdir.join("snapshots/.s2.staging-123"),
            root.join(&d.id).join("dataset.tmp-999"),
        ];
        for p in &left {
            if p.extension().is_some_and(|e| e.to_string_lossy().starts_with("tmp-")) {
                std::fs::write(p, "half").unwrap();
            } else {
                std::fs::create_dir_all(p).unwrap();
            }
        }
        drop(prep);
        let prep = reopen(&dir.0);
        for p in &left {
            assert!(!p.exists(), "{} is swept", p.display());
        }
        let i = prep.source(&ann, &s.id).unwrap().interrupted.unwrap();
        assert_eq!((i.job, i.cause), (JobKind::Upload, Cause::Crash));
        assert!(!sdir.join(recovery::MARKER).exists());
        assert_eq!(rows_of(&prep, &ann, &d.id).len(), 2, "what was kept is untouched");
        prep.add_upload(&ann, &s.id, csv(BUDGET)).unwrap();
        assert!(prep.source(&ann, &s.id).unwrap().interrupted.is_none(), "its next job ends it");
    }
}

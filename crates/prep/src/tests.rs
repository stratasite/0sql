//! A dataset's life, through the facade the server calls.

use super::*;
use crate::schema::ColumnType::*;
use serde_json::Value;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn prep() -> (Dir, Prep) {
    let dir = std::env::temp_dir().join(format!("zsql-prep-tests-{}-{}", std::process::id(), time::nanos()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut settings = Settings::defaults(&dir);
    settings.engine.memory = "256MB".into();
    settings.engine.threads = 1;
    let prep = Prep::open(&dir, settings).unwrap();
    (Dir(dir), prep)
}

fn scope(owner: &str) -> Scope {
    Scope { org: "default".into(), project: "shop".into(), owner: Owner::of(owner).unwrap() }
}

fn csv(text: &str) -> Upload<'_> {
    Upload { file_name: Some("budget.csv".into()), bytes: text.as_bytes(), options: CsvOptions::default() }
}

const BUDGET: &str = "region,month,amount\nEMEA,2026-01-01,100\nAPAC,2026-01-01,80\n";

fn json_rows(page: &Page) -> Vec<Value> {
    zsql_exec::json::rows(&page.rows).unwrap()
}

#[test]
fn an_upload_becomes_a_dataset_with_a_live_version() {
    let (dir, prep) = prep();
    let ann = scope("ann");
    let d = prep.create(&ann, None, csv(BUDGET)).unwrap();
    assert!(is_id(&d.id));
    assert_eq!(d.name, "budget");
    assert_eq!((d.recipe, d.live, d.rows), (1, Some(1), 2));
    let types: Vec<(&str, ColumnType, Role)> = d.columns.iter().map(|c| (c.name.as_str(), c.ty, c.role)).collect();
    assert_eq!(types, [("region", Text, Role::Dimension), ("month", Date, Role::Dimension), ("amount", Integer, Role::Measure)]);

    let base = dir.0.join("default/shop").join(DIR).join(&d.id);
    for f in [
        "dataset.json",
        "recipe/r1.json",
        "sources/s1/1.arrow",
        "sources/s1/1.json",
        "v1/rows.arrow",
        "v1/manifest.json",
        "v1/lineage.json",
    ] {
        assert!(base.join(f).exists(), "{f}");
    }
    let page = prep.page(&ann, &d.id, None, 0, 10, &View::default()).unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(json_rows(&page)[0], serde_json::json!(["EMEA", "2026-01-01", 100]));
    let page = prep.page(&ann, &d.id, None, 1, 10, &View::default()).unwrap();
    assert_eq!(json_rows(&page).len(), 1);

    let lineage = prep.lineage(&ann, &d.id, None).unwrap();
    assert_eq!((lineage.version, lineage.recipe), (1, 1));
    assert_eq!(lineage.sources[0].file_name.as_deref(), Some("budget.csv"));
    assert_eq!(lineage.sources[0].sha256.len(), 64);
    assert_eq!(lineage.columns[2].from[0].column, "amount");
    assert!(lineage.made_by.duckdb.starts_with('v'));
}

#[test]
fn another_owner_finds_nothing() {
    let (_dir, prep) = prep();
    let d = prep.create(&scope("ann"), None, csv(BUDGET)).unwrap();
    let bob = scope("bob");
    assert!(matches!(prep.get(&bob, &d.id), Err(Error::NotFound(_))));
    assert!(matches!(prep.page(&bob, &d.id, None, 0, 1, &View::default()), Err(Error::NotFound(_))));
    assert!(matches!(prep.delete(&bob, &d.id), Err(Error::NotFound(_))));
    assert!(prep.list(&bob).is_empty());
    assert_eq!(prep.list(&scope("ann")).len(), 1);
    assert!(Owner::of(" ").is_err());
    // The reference is never kept, only its hash.
    let text = std::fs::read_to_string(_dir.0.join("default/shop").join(DIR).join(&d.id).join("dataset.json")).unwrap();
    assert!(!text.contains("\"ann\""));
    // Another project of the same owner is another place.
    let elsewhere = Scope { project: "other".into(), ..scope("ann") };
    assert!(prep.get(&elsewhere, &d.id).is_err());
    assert!(matches!(prep.get(&scope("ann"), "../../etc"), Err(Error::NotFound(_))));
}

#[test]
fn a_replacement_is_a_new_snapshot_and_a_new_version() {
    let (_dir, prep) = prep();
    let ann = scope("ann");
    let d = prep.create(&ann, None, csv(BUDGET)).unwrap();
    let d = prep.replace(&ann, &d.id, csv("region,month,amount,owner\nEMEA,2026-01-01,100.5,kim\n")).unwrap();
    assert_eq!((d.live, d.sources[0].snapshot, d.rows), (Some(2), 2, 1));
    assert_eq!(d.columns[2].ty, Decimal(1));
    assert!(d.notes.iter().any(|n| n.contains("widened")), "{:?}", d.notes);
    assert!(d.notes.iter().any(|n| n.contains("'owner' is new")));
    // Every version is still there.
    assert_eq!(prep.page(&ann, &d.id, Some(1), 0, 10, &View::default()).unwrap().total, 2);
    assert_eq!(prep.lineage(&ann, &d.id, Some(2)).unwrap().sources[0].snapshot, 2);

    // A snapshot whose values will not convert changes nothing.
    let e = prep.replace(&ann, &d.id, csv("region,month,amount\nEMEA,soon,1\n")).err().unwrap();
    assert!(matches!(e, Error::Unprocessable(_)) && e.to_string().contains("'month' is date"), "{e}");
    let after = prep.get(&ann, &d.id).unwrap();
    assert_eq!((after.live, after.sources[0].snapshot), (Some(2), 2));
}

#[test]
fn roles_change_through_a_recipe_version_made_against_the_latest() {
    let (_dir, prep) = prep();
    let ann = scope("ann");
    let d = prep.create(&ann, None, csv(BUDGET)).unwrap();
    let mut roles = std::collections::BTreeMap::new();
    roles.insert("amount".to_string(), Role::Dimension);
    let d2 = prep.change(&ann, &d.id, Change { key: vec![], against: 1, roles: roles.clone(), steps: vec![] }).unwrap();
    assert_eq!((d2.recipe, d2.live), (2, Some(2)));
    assert_eq!(d2.columns[2].role, Role::Dimension);
    let stale = prep.change(&ann, &d.id, Change { key: vec![], against: 1, roles: roles.clone(), steps: vec![] });
    assert!(matches!(stale, Err(Error::Conflict(_))));
    roles.insert("nope".to_string(), Role::Measure);
    assert!(matches!(prep.change(&ann, &d.id, Change { key: vec![], against: 2, roles, steps: vec![] }), Err(Error::Invalid(_))));
    // Undo is the old version, which is kept.
    assert!(prep.recipe(&ann, &d.id, Some(1)).unwrap().roles.is_empty());
    // A new snapshot keeps the user's role.
    let d3 = prep.replace(&ann, &d.id, csv(BUDGET)).unwrap();
    assert_eq!(d3.columns[2].role, Role::Dimension);
}

#[test]
fn the_caps_hold_at_import_and_after() {
    let (dir, _) = prep();
    let mut settings = Settings::defaults(&dir.0);
    settings.max_rows = 1;
    let prep = Prep::open(&dir.0, settings).unwrap();
    let e = prep.create(&scope("ann"), None, csv(BUDGET)).err().unwrap();
    assert!(e.to_string().contains("more than 1 rows"), "{e}");
    let mut settings = Settings::defaults(&dir.0);
    settings.max_bytes = 10;
    let prep = Prep::open(&dir.0, settings).unwrap();
    assert!(prep.create(&scope("ann"), None, csv(BUDGET)).err().unwrap().to_string().contains("bytes"));
    // A failed creation leaves nothing behind.
    let root = dir.0.join("default/shop").join(DIR);
    assert_eq!(std::fs::read_dir(&root).map(|r| r.count()).unwrap_or(0), 0);
}

#[test]
fn deletes_are_hard() {
    let (dir, prep) = prep();
    let ann = scope("ann");
    let a = prep.create(&ann, None, csv(BUDGET)).unwrap();
    let b = prep.create(&Scope { project: "other".into(), ..scope("ann") }, None, csv(BUDGET)).unwrap();
    let c = prep.create(&scope("bob"), None, csv(BUDGET)).unwrap();
    prep.delete(&ann, &a.id).unwrap();
    assert!(!dir.0.join("default/shop").join(DIR).join(&a.id).exists());
    assert!(matches!(prep.get(&ann, &a.id), Err(Error::NotFound(_))));
    let a2 = prep.create(&ann, None, csv(BUDGET)).unwrap();
    let mut removed = prep.delete_owner("default", &Owner::of("ann").unwrap()).unwrap();
    removed.sort();
    let mut want = vec![a2.id, b.id];
    want.sort();
    assert_eq!(removed, want);
    assert_eq!(prep.get(&scope("bob"), &c.id).unwrap().id, c.id, "another owner's stays");
    assert_eq!(prep.delete_project("default", "shop").unwrap(), vec![c.id]);
    assert!(!dir.0.join("default/shop").join(DIR).exists());
}

fn steps(v: Value) -> Vec<Step> {
    serde_json::from_value(v).unwrap()
}

const SALES: &str = "region,store,amount\nNY,a,10\nCA,b,5\nNY,c,7\n";

#[test]
fn a_recipe_of_steps_makes_each_version() {
    let (_dir, prep) = prep();
    let ann = scope("ann");
    let d = prep.create(&ann, None, csv(SALES)).unwrap();
    let change = Change {
        key: vec![],
        against: 1,
        roles: Default::default(),
        steps: steps(serde_json::json!([
            { "op": "remap", "column": "region", "values": [{ "from": "NY", "to": "New York" }] },
            { "op": "group", "by": ["region"], "aggregates": [{ "column": "amount", "fn": "sum", "as": "total" }] },
        ])),
    };
    let d = prep.change(&ann, &d.id, change).unwrap();
    assert_eq!((d.recipe, d.live, d.rows), (2, Some(2), 2));
    let page = prep.page(&ann, &d.id, None, 0, 10, &View::default()).unwrap();
    assert_eq!(json_rows(&page), [serde_json::json!(["CA", 5]), serde_json::json!(["New York", 17])]);
    // Sorted and filtered for viewing, which changes nothing kept.
    let sort = [SortKey { column: "total".into(), desc: true }];
    let page = prep.page(&ann, &d.id, None, 0, 10, &View { sort: &sort, filter: Some("total > 6") }).unwrap();
    assert_eq!((page.total, json_rows(&page)), (1, vec![serde_json::json!(["New York", 17])]));
    // Lineage follows each column through the steps.
    let lineage = prep.lineage(&ann, &d.id, None).unwrap();
    assert_eq!(lineage.columns[1].from[0].column, "amount");
    // The same recipe runs over a new snapshot.
    let d = prep.replace(&ann, &d.id, csv("region,store,amount\nNY,a,1\n")).unwrap();
    let page = prep.page(&ann, &d.id, None, 0, 10, &View::default()).unwrap();
    assert_eq!(json_rows(&page), [serde_json::json!(["New York", 1])]);
}

#[test]
fn a_column_gone_upstream_is_a_note_or_holds_the_dataset() {
    let (_dir, prep) = prep();
    let ann = scope("ann");
    let d = prep.create(&ann, None, csv(SALES)).unwrap();
    // Nothing reads `store`: its going is a note.
    let d = prep.replace(&ann, &d.id, csv("region,amount\nNY,1\n")).unwrap();
    assert!(d.held.is_none());
    assert!(d.notes.iter().any(|n| n.contains("'store' is not in this snapshot")), "{:?}", d.notes);
    // A step reads `region`: its going holds the dataset, and v2 stays live.
    let change = Change {
        key: vec![],
        against: 1,
        roles: Default::default(),
        steps: steps(serde_json::json!([{ "op": "formula", "column": "label", "formula": "upper(region)" }])),
    };
    let d = prep.change(&ann, &d.id, change).unwrap();
    assert_eq!(d.live, Some(3));
    let d = prep.replace(&ann, &d.id, csv("area,amount\nNY,1\n")).unwrap();
    let held = d.held.clone().expect("held");
    assert_eq!(held.unresolved[0].describe(), "step 1 (formula) reads column 'region', which is not there at that step");
    assert_eq!(d.live, Some(3), "what was live stays live");
    // Confirming drops the step and runs the rest.
    let d = prep.confirm(&ann, &d.id, 2).unwrap();
    assert!(d.held.is_none());
    assert_eq!(d.live, Some(4));
    // A column new upstream comes after the ones the source had.
    assert_eq!(d.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["amount", "area"]);
    assert!(prep.recipe(&ann, &d.id, None).unwrap().steps.is_empty());
}

#[test]
fn a_rename_upstream_is_fixed_by_pointing_the_steps_at_the_new_name() {
    let (_dir, prep) = prep();
    let ann = scope("ann");
    let d = prep.create(&ann, None, csv(SALES)).unwrap();
    let remap = |column: &str| serde_json::json!([{ "op": "remap", "column": column, "values": [{ "from": "NY", "to": "New York" }] }]);
    let d = prep.change(&ann, &d.id, Change { key: vec![], against: 1, roles: Default::default(), steps: steps(remap("region")) }).unwrap();
    let d = prep.replace(&ann, &d.id, csv("area,amount\nNY,1\n")).unwrap();
    assert!(d.held.is_some());
    let d = prep.change(&ann, &d.id, Change { key: vec![], against: 2, roles: Default::default(), steps: steps(remap("area")) }).unwrap();
    assert!(d.held.is_none());
    let page = prep.page(&ann, &d.id, None, 0, 10, &View::default()).unwrap();
    assert_eq!(json_rows(&page), [serde_json::json!([1, "New York"])]);
}

#[test]
fn a_removal_with_readers_is_refused_naming_them() {
    let (_dir, prep) = prep();
    let ann = scope("ann");
    let d = prep.create(&ann, None, csv(SALES)).unwrap();
    let with = |extra: serde_json::Value| {
        let mut v = vec![serde_json::json!({ "op": "formula", "column": "label", "formula": "region & store" })];
        v.extend(extra.as_array().cloned().unwrap_or_default());
        steps(Value::Array(v))
    };
    let d = prep
        .change(&ann, &d.id, Change { key: vec![], against: 1, roles: Default::default(), steps: with(serde_json::json!([])) })
        .unwrap();
    let impact = prep.impact(&ann, &d.id, "store").unwrap();
    assert_eq!(
        impact.iter().map(Unresolved::describe).collect::<Vec<_>>(),
        ["step 1 (formula) reads column 'store', which is not there at that step"]
    );
    assert!(prep.impact(&ann, &d.id, "amount").unwrap().is_empty());
    assert!(prep.impact(&ann, &d.id, "label").unwrap().is_empty());
    // A removal put before its reader is refused, naming it.
    let mut bad = steps(serde_json::json!([{ "op": "remove", "columns": ["store"] }]));
    bad.extend(with(serde_json::json!([])));
    let e = prep.change(&ann, &d.id, Change { key: vec![], against: 2, roles: Default::default(), steps: bad }).err().unwrap();
    assert!(e.to_string().contains("step 2 (formula) reads column 'store'"), "{e}");
    // At the end it removes the column.
    let d = prep
        .change(
            &ann,
            &d.id,
            Change {
                key: vec![],
                against: 2,
                roles: Default::default(),
                steps: with(serde_json::json!([{ "op": "remove", "columns": ["store"] }])),
            },
        )
        .unwrap();
    assert_eq!(d.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["region", "amount", "label"]);
}

#[test]
fn a_preview_runs_a_step_over_a_sample_and_keeps_nothing() {
    let (_dir, prep) = prep();
    let ann = scope("ann");
    let d = prep.create(&ann, None, csv(SALES)).unwrap();
    let p = prep
        .preview(
            &ann,
            &d.id,
            Preview { steps: Some(steps(serde_json::json!([{ "op": "filter", "formula": "amount > 6" }]))), ..Preview::default() },
        )
        .unwrap();
    assert_eq!(p.rows.num_rows(), 2);
    let p = prep
        .preview(
            &ann,
            &d.id,
            Preview { steps: Some(steps(serde_json::json!([{ "op": "trim", "columns": ["gone"] }]))), ..Preview::default() },
        )
        .unwrap();
    assert_eq!(p.unresolved.len(), 1, "a preview shows what will not resolve rather than refusing");
    assert_eq!(prep.get(&ann, &d.id).unwrap().made, 1, "nothing was made");
    let e = prep
        .preview(
            &ann,
            &d.id,
            Preview {
                steps: Some(steps(serde_json::json!([{ "op": "retype", "column": "region", "type": "integer" }]))),
                ..Preview::default()
            },
        )
        .err()
        .unwrap();
    assert!(e.to_string().contains("do not convert without loss, such as 'NY'"), "{e}");
}

fn edits(against: u32, v: Value) -> edits::Request {
    edits::Request { against, edits: serde_json::from_value(v).unwrap() }
}

const BUDGETS: &str = "region,month,amount\nNY,2026-01-01,100\nCA,2026-01-01,80\nNY,2026-02-01,90\n";

fn rows_of(prep: &Prep, scope: &Scope, id: &str) -> Vec<Value> {
    json_rows(&prep.page(scope, id, None, 0, 100, &View::default()).unwrap())
}

#[test]
fn a_dimension_edit_is_a_remap_and_holds_through_a_sync() {
    let (_dir, prep) = prep();
    let ann = scope("ann");
    let d = prep.create(&ann, None, csv(BUDGETS)).unwrap();
    let (d, out) = prep
        .edit(&ann, &d.id, edits(1, serde_json::json!([{ "op": "cell", "column": "region", "from": "NY", "to": "New York" }])))
        .unwrap();
    assert_eq!((d.edits, d.recipe, d.live), (1, 1, Some(2)), "an edit is a log entry, not a recipe version");
    assert_eq!(out[0].state, State::Applied);
    let rows = rows_of(&prep, &ann, &d.id);
    assert_eq!(rows[0][0], "New York");
    assert_eq!(rows[2][0], "New York");
    let d = prep.replace(&ann, &d.id, csv("region,month,amount\nNY,2026-03-01,1\n")).unwrap();
    assert_eq!(rows_of(&prep, &ann, &d.id), [serde_json::json!(["New York", "2026-03-01", 1])]);
    assert_eq!(prep.lineage(&ann, &d.id, None).unwrap().edits, 1);
}

#[test]
fn a_measure_edit_overrides_one_row_and_is_flagged_when_upstream_moves() {
    let (_dir, prep) = prep();
    let ann = scope("ann");
    let d = prep.create(&ann, None, csv(BUDGETS)).unwrap();
    let row = serde_json::json!({ "region": "NY", "month": "2026-01-01", "amount": 100 });
    let (d, out) =
        prep.edit(&ann, &d.id, edits(1, serde_json::json!([{ "op": "cell", "column": "amount", "row": row, "to": 120 }]))).unwrap();
    assert_eq!(out[0].state, State::Applied);
    assert_eq!(rows_of(&prep, &ann, &d.id)[0], serde_json::json!(["NY", "2026-01-01", 120]));
    // Upstream moves the value it replaced: the user's value wins, flagged.
    let d = prep.replace(&ann, &d.id, csv("region,month,amount\nNY,2026-01-01,105\nCA,2026-01-01,80\n")).unwrap();
    assert_eq!(rows_of(&prep, &ann, &d.id)[0], serde_json::json!(["NY", "2026-01-01", 120]));
    let (_, outcomes) = prep.edit_log(&ann, &d.id).unwrap();
    assert_eq!(outcomes[0].state, State::Flagged, "{outcomes:?}");
    assert!(d.notes.iter().any(|n| n.contains("flagged")), "{:?}", d.notes);
    // The row goes upstream: orphaned, kept, not applied.
    let d = prep.replace(&ann, &d.id, csv("region,month,amount\nCA,2026-01-01,80\n")).unwrap();
    let (log, outcomes) = prep.edit_log(&ann, &d.id).unwrap();
    assert_eq!((log.len(), outcomes[0].state.clone()), (1, State::Orphaned));
    // It comes back, and so does the edit.
    let d = prep.replace(&ann, &d.id, csv("region,month,amount\nNY,2026-01-01,100\n")).unwrap();
    assert_eq!(rows_of(&prep, &ann, &d.id)[0][2], 120);
}

#[test]
fn an_edit_that_does_not_name_one_row_is_refused_unless_a_key_or_all_says() {
    let (_dir, prep) = prep();
    let ann = scope("ann");
    let d = prep.create(&ann, None, csv("region,store,amount\nNY,a,1\nNY,a,2\nCA,b,3\n")).unwrap();
    let row = serde_json::json!({ "region": "NY", "store": "a" });
    let e = prep.edit(&ann, &d.id, edits(1, serde_json::json!([{ "op": "cell", "column": "amount", "row": row, "to": 9 }]))).err().unwrap();
    assert!(e.to_string().contains("2 rows have these values") && e.to_string().contains("mark a key"), "{e}");
    let e = prep
        .edit(
            &ann,
            &d.id,
            edits(1, serde_json::json!([{ "op": "cell", "column": "amount", "row": { "region": "TX", "store": "z" }, "to": 9 }])),
        )
        .err();
    assert!(e.unwrap().to_string().contains("no row has these values"));
    let (d, _) = prep
        .edit(&ann, &d.id, edits(1, serde_json::json!([{ "op": "cell", "column": "amount", "row": row, "to": 9, "all": true }])))
        .unwrap();
    let amounts: Vec<Value> = rows_of(&prep, &ann, &d.id).iter().map(|r| r[2].clone()).collect();
    assert_eq!(amounts, [9, 9, 3]);
}

#[test]
fn a_dimension_edit_for_one_row_rows_added_and_deleted_and_undo() {
    let (_dir, prep) = prep();
    let ann = scope("ann");
    let d = prep.create(&ann, None, csv(BUDGETS)).unwrap();
    let (d, out) = prep
        .edit(
            &ann,
            &d.id,
            edits(
                1,
                serde_json::json!([
                    { "op": "cell", "column": "region", "row": { "region": "NY", "month": "2026-02-01" }, "to": "NJ", "one": true },
                    { "op": "delete", "row": { "region": "CA", "month": "2026-01-01" } },
                    { "op": "add", "values": { "region": "TX", "month": "2026-01-01", "amount": 5 } },
                ]),
            ),
        )
        .unwrap();
    assert!(out.iter().all(|o| o.state == State::Applied), "{out:?}");
    let rows = rows_of(&prep, &ann, &d.id);
    assert_eq!(
        rows,
        [
            serde_json::json!(["NY", "2026-01-01", 100]),
            serde_json::json!(["NJ", "2026-02-01", 90]),
            serde_json::json!(["TX", "2026-01-01", 5]),
        ]
    );
    let e =
        prep.edit(&ann, &d.id, edits(1, serde_json::json!([{ "op": "add", "values": { "region": "NY", "month": "2026-01-01" } }]))).err();
    assert!(e.unwrap().to_string().contains("is there already"));
    let (d, _) = prep.edit(&ann, &d.id, edits(1, serde_json::json!([{ "op": "undo", "edit": "1.2" }]))).unwrap();
    assert_eq!(rows_of(&prep, &ann, &d.id).len(), 4, "the deleted row is back");
    let e = prep.edit(&ann, &d.id, edits(1, serde_json::json!([{ "op": "undo", "edit": "1.2" }]))).err().unwrap();
    assert!(e.to_string().contains("undone already"), "{e}");
    assert!(matches!(prep.edit(&ann, &d.id, edits(2, serde_json::json!([{ "op": "undo", "edit": "1.1" }]))), Err(Error::Conflict(_))));
}

#[test]
fn edits_apply_where_they_were_made_and_an_edit_on_a_gone_column_holds() {
    let (_dir, prep) = prep();
    let ann = scope("ann");
    let d = prep.create(&ann, None, csv(BUDGETS)).unwrap();
    let (d, _) = prep
        .edit(&ann, &d.id, edits(1, serde_json::json!([{ "op": "cell", "column": "region", "from": "NY", "to": "New York" }])))
        .unwrap();
    // A step added later runs after the edit: it sees the remapped value.
    let change = Change {
        key: vec![],
        against: 1,
        roles: Default::default(),
        steps: steps(serde_json::json!([{ "op": "filter", "formula": "region = 'New York'" }])),
    };
    let d = prep.change(&ann, &d.id, change).unwrap();
    assert_eq!(d.rows, 2, "the filter runs after the edit made before it");
    // The edited column goes upstream: the edit holds the dataset; confirming undoes it.
    let d = prep.replace(&ann, &d.id, csv("area,month,amount\nNY,2026-01-01,1\n")).unwrap();
    let held = d.held.clone().unwrap();
    assert!(held.unresolved.iter().any(|u| u.edit.as_deref() == Some("1.1")), "{:?}", held.unresolved);
    let d = prep.confirm(&ann, &d.id, 2).unwrap();
    assert!(d.held.is_none());
    let (log, _) = prep.edit_log(&ann, &d.id).unwrap();
    assert!(matches!(&log.last().unwrap().edits[0], Edit::Undo { undoes, .. } if undoes == "1.1"));
}

fn arrived(regions: &[&str], revenue: &[i128]) -> Arrived {
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
            spec: Some(serde_json::json!({ "fields": ["region", "year", "revenue"] })),
            expr: None,
            sql_sha256: "ab".repeat(32),
            context: "cd".repeat(32),
            fields: [("revenue".to_string(), "orders.revenue".to_string())].into_iter().collect(),
        },
    }
}

#[test]
fn a_semantic_query_is_a_source_refreshed_by_request() {
    let (_dir, prep) = prep();
    let ann = scope("ann");
    let query = Query { branch: "main".into(), spec: Some(serde_json::json!({ "fields": ["region", "revenue"] })), expr: None };
    let id = new_id("shop");
    let d = prep.create_from_query(&ann, &id, Some("revenue"), query.clone(), arrived(&["NY", "CA"], &[1050, 200])).unwrap();
    assert_eq!(d.id, id);
    let kinds: Vec<(&str, ColumnType, Role)> = d.columns.iter().map(|c| (c.name.as_str(), c.ty, c.role)).collect();
    assert_eq!(kinds, [("region", Text, Role::Dimension), ("year", Integer, Role::Dimension), ("revenue", Decimal(2), Role::Measure)]);
    assert_eq!(prep.query(&ann, &id).unwrap(), query);
    let lineage = prep.lineage(&ann, &id, None).unwrap();
    let read = lineage.sources[0].semantic.as_ref().unwrap();
    assert_eq!((read.branch.as_str(), read.fields["revenue"].as_str()), ("main", "orders.revenue"));
    // A refresh is a new snapshot, and the recipe runs on it.
    let d = prep
        .change(
            &ann,
            &id,
            Change {
                key: vec![],
                against: 1,
                roles: Default::default(),
                steps: steps(serde_json::json!([{ "op": "remove", "columns": ["year"] }])),
            },
        )
        .unwrap();
    let d = prep.refresh(&ann, &d.id, arrived(&["NY"], &[99])).unwrap();
    assert_eq!((d.sources[0].snapshot, d.rows), (2, 1));
    assert_eq!(rows_of(&prep, &ann, &id), [serde_json::json!(["NY", "0.99"])]);
    assert!(prep.replace(&ann, &id, csv(BUDGET)).is_err(), "a query's dataset is not replaced by an upload");
    assert!(prep.create_from_query(&ann, &id, None, query, arrived(&[], &[])).is_err(), "an id is made once");
}

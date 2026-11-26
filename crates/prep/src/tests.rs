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
    let page = prep.page(&ann, &d.id, None, 0, 10).unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(json_rows(&page)[0], serde_json::json!(["EMEA", "2026-01-01", 100]));
    let page = prep.page(&ann, &d.id, None, 1, 10).unwrap();
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
    assert!(matches!(prep.page(&bob, &d.id, None, 0, 1), Err(Error::NotFound(_))));
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
    assert_eq!(prep.page(&ann, &d.id, Some(1), 0, 10).unwrap().total, 2);
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
    let d2 = prep.change(&ann, &d.id, Change { against: 1, roles: roles.clone(), steps: vec![] }).unwrap();
    assert_eq!((d2.recipe, d2.live), (2, Some(2)));
    assert_eq!(d2.columns[2].role, Role::Dimension);
    let stale = prep.change(&ann, &d.id, Change { against: 1, roles: roles.clone(), steps: vec![] });
    assert!(matches!(stale, Err(Error::Conflict(_))));
    roles.insert("nope".to_string(), Role::Measure);
    assert!(matches!(prep.change(&ann, &d.id, Change { against: 2, roles, steps: vec![] }), Err(Error::Invalid(_))));
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

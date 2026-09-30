//! Scoreboard over the golden cases. Prints per-file counts; fails only when
//! `ZSQL_GOLDEN_STRICT` is set. `ZSQL_GOLDEN_FILTER` narrows to case ids
//! containing the value, `ZSQL_PATHS=snapshot` plans over the stored paths.

use zsql_golden::{print_scoreboard, score, Fixtures, PathSource};

#[test]
fn planner_scoreboard() {
    let fx = Fixtures::open(Fixtures::default_root());
    let filter = std::env::var("ZSQL_GOLDEN_FILTER").unwrap_or_default();
    let report = score(&fx, &filter, PathSource::from_env()).unwrap();
    let (total, passed) = print_scoreboard(&report);
    for fs in report.values() {
        for (id, outcome) in fs.failed.iter().take(3) {
            println!("  FAIL {id}: {outcome:?}");
        }
    }
    if std::env::var("ZSQL_GOLDEN_STRICT").is_ok() {
        assert_eq!(passed, total, "golden cases failing");
    }
}

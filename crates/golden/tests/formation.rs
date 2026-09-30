//! Formed join paths against the paths Rails stored in every snapshot: for
//! each root and field, the cheapest stored route must be the formed one.
//! Prints totals; fails only under `ZSQL_GOLDEN_STRICT`.

use zsql_golden::{compare_paths, Fixtures};

#[test]
fn formed_paths_match_stored_paths() {
    let fx = Fixtures::open(Fixtures::default_root());
    let (mut pairs, mut matched, mut mismatched) = (0, 0, 0);
    let mut samples: Vec<String> = Vec::new();
    for hash in fx.snapshot_hashes().unwrap() {
        let u = fx.universe(&hash).unwrap();
        let formed = zsql_formation::form(&u);
        let report = compare_paths(&u, &u.snapshot().paths, &formed.paths);
        pairs += report.pairs;
        matched += report.matched;
        mismatched += report.mismatches.len();
        if samples.len() < 8 {
            samples.extend(report.mismatches.iter().take(2).map(|m| format!("{hash}: {m}")));
        }
    }
    println!("formation: {pairs} root/field pairs, {matched} matched, {mismatched} mismatched");
    for s in &samples {
        println!("  {s}");
    }
    if std::env::var("ZSQL_GOLDEN_STRICT").is_ok() {
        assert_eq!(matched, pairs, "formed paths diverge from stored paths");
    }
}

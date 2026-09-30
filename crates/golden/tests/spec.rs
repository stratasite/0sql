//! The spec contract: a spec serialized back from its typed query lowers to
//! the same plan, fields resolve by name as well as by uid, and bad
//! references are refused by name.

use zsql_golden::{run_case_on, Fixtures, PathSource};
use zsql_spec::{lower, resolve_field, Spec, SpecError};

#[test]
fn specs_serialize_back_and_lower_to_the_same_plan() {
    let fx = Fixtures::open(Fixtures::default_root());
    let dialects = fx.dialects();
    let base = dialects.base().unwrap();
    let (mut checked, mut skipped) = (0, 0);
    for (_, cases) in fx.cases().unwrap() {
        for case in &cases {
            let (universe, paths) = fx.planner_for(&case.universe, PathSource::Formed).unwrap();
            let planner = zsql_planner::Planner::new(&universe, dialects, paths.as_deref().map(Vec::as_slice));
            if !run_case_on(&planner, dialects, case).unwrap().passed() {
                skipped += 1;
                continue;
            }
            let spec = Spec::from_value(case.spec.clone()).unwrap();
            let first = lower(&spec, &universe, &base).unwrap();
            let again = lower(&first.to_spec(), &universe, &base).unwrap_or_else(|e| panic!("{}: re-lowering its own spec: {e}", case.id));
            let a = planner.plan_query(first, case.context.as_ref());
            let b = planner.plan_query(again, case.context.as_ref());
            match (a, b) {
                (Ok(a), Ok(b)) => assert_eq!(a.sql, b.sql, "{}: spec -> query -> spec changes the plan", case.id),
                (Err(a), Err(b)) => assert_eq!(a.rails_class(), b.rails_class(), "{}", case.id),
                (a, b) => panic!("{}: outcomes differ: {:?} vs {:?}", case.id, a.map(|p| p.sql), b.map(|p| p.sql)),
            }
            checked += 1;
        }
    }
    eprintln!("spec roundtrip: {checked} checked, {skipped} skipped (case does not pass)");
    assert!(checked > 0);
}

#[test]
fn fields_resolve_by_name_as_well_as_uid() {
    let fx = Fixtures::open(Fixtures::default_root());
    let dialects = fx.dialects();
    let base = dialects.base().unwrap();
    let mut checked = 0;
    for (_, cases) in fx.cases().unwrap() {
        for case in cases.iter().filter(|c| matches!(c.expected, zsql_golden::Expected::Sql { .. })) {
            let (universe, paths) = fx.planner_for(&case.universe, PathSource::Formed).unwrap();
            let planner = zsql_planner::Planner::new(&universe, dialects, paths.as_deref().map(Vec::as_slice));
            if !run_case_on(&planner, dialects, case).unwrap().passed() {
                continue;
            }
            let spec = Spec::from_value(case.spec.clone()).unwrap();
            // Every projection's uid becomes the field's name with its kind,
            // so a name shared by a dimension and a measure stays unambiguous.
            let mut by_name = spec.clone();
            for p in by_name.projections.iter_mut() {
                if let Some(serde_json::Value::String(uid)) = &p.field {
                    let f = resolve_field(&universe, uid, p.field_type.as_deref()).unwrap();
                    let suffix = match f.kind {
                        zsql_semantic::FieldKind::Dimension => "@d",
                        zsql_semantic::FieldKind::Measure => "@m",
                    };
                    p.field = Some(serde_json::Value::String(format!("{}{}", f.name, suffix)));
                    p.field_type = None;
                }
            }
            let a = planner.plan_query(lower(&spec, &universe, &base).unwrap(), case.context.as_ref()).unwrap();
            let b = planner.plan_query(lower(&by_name, &universe, &base).unwrap(), case.context.as_ref()).unwrap();
            assert_eq!(a.sql, b.sql, "{}: naming the fields changes the plan", case.id);
            checked += 1;
        }
    }
    assert!(checked > 0);
}

#[test]
fn unresolvable_references_are_named_in_the_error() {
    let fx = Fixtures::open(Fixtures::default_root());
    let hash = fx.snapshot_hashes().unwrap().into_iter().next().unwrap();
    let universe = fx.universe(&hash).unwrap();
    match resolve_field(&universe, "no_such_field_anywhere", None) {
        Err(SpecError::NotFound(msg)) => assert!(msg.contains("no_such_field_anywhere"), "{msg}"),
        other => panic!("expected NotFound, got {other:?}"),
    }
    let f = universe.fields().first().unwrap();
    let spelled = format!("  {}  ", f.uid.to_uppercase());
    assert_eq!(resolve_field(&universe, &spelled, None).unwrap().uid, f.uid);
    if let Some(h) = universe.fields().iter().find(|f| f.hidden) {
        assert_eq!(resolve_field(&universe, &h.uid, None).unwrap().uid, h.uid);
    }
    let bad = Spec::from_value(serde_json::json!({ "projections": [], "filtres": [] }));
    assert!(matches!(bad, Err(SpecError::Invalid(ref m)) if m.contains("filtres")), "{bad:?}");
}

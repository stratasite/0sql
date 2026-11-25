use crate::builder::{measure_has_function, parse_format_syntax, swap_sides};
use crate::uid::{parameterize, Slugs};

#[test]
fn parameterize_matches_active_support() {
    assert_eq!(parameterize("Store Net Paid"), "store-net-paid");
    assert_eq!(parameterize("ss_net_paid"), "ss_net_paid");
    assert_eq!(parameterize("  % Margin (net) "), "margin-net");
    assert_eq!(parameterize("Enrollment (K-12)"), "enrollment-k-12");
}

#[test]
fn slugs_fall_back_to_the_next_candidate() {
    let mut s = Slugs::new();
    assert_eq!(s.take(&["Date".into(), "dimension-Date".into()]), "date");
    assert_eq!(s.take(&["Date".into(), "measure-Date".into()]), "measure-date");
    assert_eq!(s.take(&["Date".into(), "measure-Date".into()]), "date-2");
}

#[test]
fn join_sides_swap() {
    assert_eq!(swap_sides("left.id = right.customer_id"), "right.id = left.customer_id");
}

#[test]
fn format_shortcuts() {
    assert_eq!(parse_format_syntax("currency:2"), Some(serde_json::json!({"type": "currency", "precision": 2})));
    assert_eq!(parse_format_syntax("number:2:abbreviate"), Some(serde_json::json!({"type": "number", "precision": 2, "abbreviate": true})));
    assert_eq!(parse_format_syntax("date:short"), Some(serde_json::json!({"type": "date", "pattern": "%b %d, %Y"})));
    assert_eq!(parse_format_syntax("nope"), None);
}

#[test]
fn measure_function_rule() {
    assert!(measure_has_function("sum(ss_net_paid)"));
    assert!(measure_has_function("count(distinct x) * 1.0"));
    assert!(!measure_has_function("ss_net_paid"));
}

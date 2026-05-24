//! Threshold dominance extensions from paper Section 4.6.

use passant_core::threshold_dominates;

#[test]
fn count_distinct_equality_policies_dominance() {
    assert!(threshold_dominates(
        "count(distinct foo.id) = 1",
        "count(distinct foo.id) = 2"
    ));
    assert!(!threshold_dominates(
        "count(distinct foo.id) = 2",
        "count(distinct foo.id) = 1"
    ));
}

#[test]
fn count_distinct_inequality_policies_dominance() {
    assert!(threshold_dominates(
        "count(distinct foo.id) != 1",
        "count(distinct foo.id) != 2"
    ));
}

#[test]
fn parametric_k_anonymity_template_dominance() {
    assert!(threshold_dominates(
        "count(distinct receipts.customer_id) >= 5",
        "count(distinct receipts.customer_id) >= 3"
    ));
}

#[test]
fn mixed_comparison_operators_do_not_false_positive() {
    assert!(!threshold_dominates("max(foo.id) > 10", "max(foo.id) = 10"));
    assert!(!threshold_dominates("max(foo.id) = 10", "max(foo.id) > 10"));
}

#[test]
fn planner_applies_dominance_before_rewrite() {
    use passant_core::{PolicyIr, Resolution};

    use crate::common::{plan_query, rewrite};

    let policies = vec![
        PolicyIr::CompatDfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "count(distinct foo.id) = 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        },
        PolicyIr::CompatDfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "count(distinct foo.id) = 3".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        },
    ];
    let result = plan_query("SELECT id FROM foo", &policies);
    assert_eq!(result.applicable_policies.len(), 1);
    let sql = rewrite("SELECT id FROM foo", &policies);
    assert_eq!(sql, "SELECT id FROM foo WHERE foo.id IS NOT NULL");
}

//! Explain output must reflect the executable rewrite strategy.

use passant_core::{PassantPlanner, PolicyIr, Resolution, RewriteStrategy, parse_query_to_ir};

fn explain_strategy(sql: &str, policies: &[PolicyIr]) -> RewriteStrategy {
    let ir = parse_query_to_ir(sql).expect("query should parse");
    PassantPlanner::new()
        .plan_query(&ir, policies)
        .chosen
        .strategy
}

#[test]
fn explain_selects_full_push_for_distributive_scan() {
    let strategy = explain_strategy(
        "SELECT id FROM foo",
        &[PolicyIr::CompatDfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(foo.id) > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
    );
    assert_eq!(strategy, RewriteStrategy::FullPush);
}

#[test]
fn explain_selects_partial_push_for_non_distributive_policy() {
    let strategy = explain_strategy(
        "SELECT id FROM foo",
        &[PolicyIr::CompatDfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "avg(foo.id) > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
    );
    assert_eq!(strategy, RewriteStrategy::PartialPush);
}

#[test]
fn explain_selects_full_push_for_non_monotonic_set_operation() {
    let strategy = explain_strategy(
        "SELECT id FROM bar EXCEPT SELECT id FROM foo",
        &[PolicyIr::CompatDfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(foo.id) > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
    );
    assert_eq!(strategy, RewriteStrategy::FullPush);
}

#[test]
fn explain_includes_strategy_reasons_for_distributive_scan() {
    let ir = parse_query_to_ir("SELECT id FROM foo").expect("parse");
    let policies = vec![PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];
    let result = PassantPlanner::new().plan_query(&ir, &policies);
    assert_eq!(result.chosen.strategy, RewriteStrategy::FullPush);
    assert!(
        result
            .chosen
            .strategy_reasons
            .iter()
            .any(|reason| reason.contains("semiring")),
        "expected semiring reason: {:?}",
        result.chosen.strategy_reasons
    );
}

#[test]
fn explain_partial_push_includes_non_distributive_reason() {
    let ir = parse_query_to_ir("SELECT id FROM foo").expect("parse");
    let policies = vec![PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "avg(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];
    let result = PassantPlanner::new().plan_query(&ir, &policies);
    assert_eq!(result.chosen.strategy, RewriteStrategy::PartialPush);
    assert!(
        result
            .chosen
            .strategy_reasons
            .iter()
            .any(|reason| reason.contains("non-distributive")),
        "expected partial-push reason: {:?}",
        result.chosen.strategy_reasons
    );
}

#[test]
fn explain_records_rewrite_error_for_delete_with_policies() {
    let ir = parse_query_to_ir("DELETE FROM foo WHERE id = 1").expect("parse");
    let policies = vec![PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];
    let explanation = PassantPlanner::new().explain_rewrite(&ir, &policies);
    assert_eq!(
        explanation.chosen.rewrite_error.as_deref(),
        Some("unsupported query form: delete with registered policies")
    );
}

#[test]
fn explain_records_source_set_scope_for_outer_join() {
    let ir = parse_query_to_ir("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id")
        .expect("parse");
    let policies = vec![PolicyIr::CompatDfc {
        sources: vec!["bar".to_string(), "foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(bar.id) > max(foo.id)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];
    let explanation = PassantPlanner::new().explain_rewrite(&ir, &policies);
    assert!(explanation.scope.requires_source_set_annotations);
    assert_eq!(explanation.chosen.strategy, RewriteStrategy::FullPush);
}

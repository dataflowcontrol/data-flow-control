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
        &[PolicyIr::Pgn {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
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
        &[PolicyIr::Pgn {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
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
        &[PolicyIr::Pgn {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
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
    let policies = vec![PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
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
    let policies = vec![PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
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
fn explain_passthrough_delete_with_policies() {
    let ir = parse_query_to_ir("DELETE FROM foo WHERE id = 1").expect("parse");
    let policies = vec![PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];
    let explanation = PassantPlanner::new().explain_rewrite(&ir, &policies);
    assert!(explanation.chosen.rewrite_error.is_none());
    assert_eq!(
        explanation.chosen.rewritten_sql.as_str(),
        "DELETE FROM foo WHERE id = 1"
    );
}

#[test]
fn explain_records_source_set_scope_for_outer_join() {
    let ir = parse_query_to_ir("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id")
        .expect("parse");
    let policies = vec![PolicyIr::Pgn {
        sources: vec!["bar".to_string(), "foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(bar.id) > max(foo.id)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];
    let explanation = PassantPlanner::new().explain_rewrite(&ir, &policies);
    assert!(explanation.scope.requires_source_set_annotations);
    assert_eq!(explanation.chosen.strategy, RewriteStrategy::FullPush);
}

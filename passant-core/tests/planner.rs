#[path = "common/mod.rs"]
mod common;

use passant_core::{
    PassantPlanner, PassantRewriter, PolicyIr, Resolution, RewriteStrategy, analyze_constraint,
    parse_query_to_ir,
};

#[test]
fn planner_chooses_full_push_for_aggregate_query() {
    let ir = parse_query_to_ir("SELECT max(foo.id) FROM foo").expect("query should parse");
    let policies = vec![PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];

    let result = PassantPlanner::new().plan_query(&ir, &policies);
    assert_eq!(result.chosen.strategy, RewriteStrategy::FullPush);
}

#[test]
fn planner_chooses_full_push_for_monotonic_spj_query() {
    let ir = parse_query_to_ir("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id")
        .expect("query should parse");
    let policies = vec![PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];

    let result = PassantPlanner::new().plan_query(&ir, &policies);
    assert_eq!(result.chosen.strategy, RewriteStrategy::FullPush);
    assert!(result.scope.has_join);
    assert!(!result.scope.has_non_monotonic_operation);
    assert!(!result.scope.requires_source_set_annotations);
    assert!(result.scope.policy_aggregates_distributive);
}

#[test]
fn semiring_analysis_classifies_policy_aggregates() {
    let aggregates = analyze_constraint("sum(foo.amount) > avg(bar.amount) AND max(foo.id) > 1")
        .expect("constraint should analyze");

    assert_eq!(aggregates.len(), 3);
    assert!(aggregates[0].distributive);
    assert!(!aggregates[1].distributive);
    assert_eq!(aggregates[1].expression, "avg(bar.amount)");
    assert!(aggregates[2].distributive);
}

#[test]
fn planner_uses_partial_push_for_non_distributive_policy_aggregate() {
    let ir = parse_query_to_ir("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id")
        .expect("query should parse");
    let policies = vec![PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "avg(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];

    let result = PassantPlanner::new().plan_query(&ir, &policies);
    assert_eq!(result.chosen.strategy, RewriteStrategy::PartialPush);
    assert_eq!(result.scope.policy_aggregate_count, 1);
    assert!(!result.scope.policy_aggregates_distributive);
    assert_eq!(
        result.scope.non_distributive_policy_aggregates,
        vec!["avg(foo.id)".to_string()]
    );
}

#[test]
fn rewriter_uses_partial_push_for_aggregate_only_non_distributive_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "avg(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo")
        .expect("avg scan policy should rewrite via partial-push");
    assert!(sql.contains("WITH base_query AS ("));
    assert!(sql.contains("policy_eval AS ("));
}

#[test]
fn rewriter_splits_source_local_non_distributive_policies_via_partial_push() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string(), "bar".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "avg(foo.id) > 1 AND avg(bar.id) > 10".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id")
        .expect("source-local non-distributive predicates should partial-push");
    assert!(sql.contains("WITH base_query AS ("));
    assert!(sql.contains("avg(foo.id) > 1"));
    assert!(sql.contains("avg(bar.id) > 10"));
}

#[test]
fn rewriter_uses_partial_push_for_alias_non_distributive_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "avg(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT f.id FROM foo AS f")
        .expect("alias non-distributive policy should partial-push");
    assert!(sql.contains("WITH base_query AS ("));
    assert!(sql.contains("avg(foo.id) > 1") || sql.contains("avg(f.id) > 1"));
}

#[test]
fn rewriter_partial_pushes_cross_source_non_distributive_aggregate_comparison() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string(), "bar".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "avg(foo.id) > avg(bar.id)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id")
        .expect("cross-source non-distributive policy should partial-push");
    assert!(sql.contains("WITH base_query AS ("));
}

#[test]
fn rewriter_partial_pushes_mixed_row_and_non_distributive_aggregate_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "foo.id > 0 AND avg(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo")
        .expect("mixed row/non-distributive policy should partial-push");
    assert!(sql.contains("WITH base_query AS ("));
}

#[test]
fn planner_chooses_full_push_for_non_monotonic_query() {
    let ir = parse_query_to_ir("SELECT id FROM bar EXCEPT SELECT id FROM foo")
        .expect("query should parse");
    let policies = vec![PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];

    let result = PassantPlanner::new().plan_query(&ir, &policies);
    assert_eq!(result.chosen.strategy, RewriteStrategy::FullPush);
    assert!(result.scope.has_non_monotonic_operation);
    assert!(result.scope.requires_source_set_annotations);
}

#[test]
fn planner_marks_outer_join_as_requiring_source_set_annotations() {
    let ir = parse_query_to_ir("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id")
        .expect("query should parse");
    let policies = vec![PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];

    let result = PassantPlanner::new().plan_query(&ir, &policies);
    assert_eq!(result.chosen.strategy, RewriteStrategy::FullPush);
    assert!(result.scope.requires_source_set_annotations);
}

#[test]
fn planner_records_successful_except_rewrite_in_explain_output() {
    let ir = parse_query_to_ir("SELECT id FROM bar EXCEPT SELECT id FROM foo")
        .expect("query should parse");
    let policies = vec![PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];

    let explanation = PassantPlanner::new().explain_rewrite(&ir, &policies);
    assert!(explanation.chosen.rewrite_error.is_none());
}

#[test]
fn planner_records_successful_source_set_rewrite_in_explain_output() {
    let ir = parse_query_to_ir("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id")
        .expect("query should parse");
    let policies = vec![PolicyIr::Pgn {
        sources: vec!["bar".to_string(), "foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(bar.id) > max(foo.id)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];

    let explanation = PassantPlanner::new().explain_rewrite(&ir, &policies);
    assert!(explanation.chosen.rewrite_error.is_none());
    assert!(explanation.scope.requires_source_set_annotations);
}

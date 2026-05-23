#[path = "common/mod.rs"]
mod common;

use passant_core::{
    AggregateDfcPolicy, PassantPlanner, PassantRewriter, PolicyIr, Resolution, RewriteStrategy,
    analyze_constraint, parse_query_to_ir,
};

#[test]
fn planner_chooses_aggregate_inline_for_aggregate_query() {
    let ir = parse_query_to_ir("SELECT max(foo.id) FROM foo").expect("query should parse");
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
    assert_eq!(result.chosen.strategy, RewriteStrategy::PartialPush);
}

#[test]
fn planner_chooses_full_push_for_monotonic_spj_query() {
    let ir = parse_query_to_ir("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id")
        .expect("query should parse");
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
    assert_eq!(result.scope.policy_aggregate_count, 1);
    assert!(!result.scope.policy_aggregates_distributive);
    assert_eq!(
        result.scope.non_distributive_policy_aggregates,
        vec!["avg(foo.id)".to_string()]
    );
}

#[test]
fn rewriter_uses_scalar_fallback_for_aggregate_only_non_distributive_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "avg(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo")
        .expect("avg scan policy should rewrite as scalar fallback");
    assert_eq!(
        sql,
        "SELECT id FROM foo WHERE (SELECT avg(foo.id) > 1 FROM foo)"
    );
}

#[test]
fn rewriter_splits_source_local_non_distributive_aggregate_fallbacks() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string(), "bar".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "avg(foo.id) > 1 AND avg(bar.id) > 10".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id")
        .expect("source-local aggregate predicates should split");
    assert_eq!(
        sql,
        "SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id WHERE (SELECT avg(foo.id) > 1 FROM foo) AND (SELECT avg(bar.id) > 10 FROM bar)"
    );
}

#[test]
fn rewriter_uses_base_source_in_alias_scalar_fallback() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "avg(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT f.id FROM foo AS f")
        .expect("alias aggregate fallback should rewrite against base table");
    assert_eq!(
        sql,
        "SELECT f.id FROM foo AS f WHERE (SELECT avg(foo.id) > 1 FROM foo)"
    );
}

#[test]
fn rewriter_rejects_cross_source_non_distributive_aggregate_comparison() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string(), "bar".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "avg(foo.id) > avg(bar.id)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let err = rewriter
        .rewrite("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id")
        .expect_err("cross-source aggregate comparison should require full fallback");
    assert_eq!(
        err.to_string(),
        "unsupported query form: non-distributive multi-source aggregate predicate requires Partial-Push or LogicalFallback"
    );
}

#[test]
fn rewriter_rejects_mixed_row_and_non_distributive_aggregate_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "foo.id > 0 AND avg(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let err = rewriter
        .rewrite("SELECT id FROM foo")
        .expect_err("mixed row/global policy should require a fuller fallback");
    assert_eq!(
        err.to_string(),
        "unsupported query form: non-distributive policy aggregate(s) require Partial-Push or LogicalFallback: avg(foo.id)"
    );
}

#[test]
fn planner_chooses_logical_fallback_for_non_monotonic_query() {
    let ir = parse_query_to_ir("SELECT id FROM bar EXCEPT SELECT id FROM foo")
        .expect("query should parse");
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
    assert_eq!(result.chosen.strategy, RewriteStrategy::LogicalFallback);
    assert!(result.scope.has_non_monotonic_operation);
    assert!(result.scope.requires_source_set_annotations);
}

#[test]
fn planner_marks_outer_join_as_requiring_source_set_annotations() {
    let ir = parse_query_to_ir("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id")
        .expect("query should parse");
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
    assert_eq!(result.chosen.strategy, RewriteStrategy::PartialPush);
    assert!(result.scope.requires_source_set_annotations);
}

#[test]
fn planner_can_defer_aggregate_policy_finalize() {
    let ir = parse_query_to_ir("INSERT INTO reports SELECT max(foo.id) AS id FROM foo")
        .expect("query should parse");
    let policies = vec![PolicyIr::CompatAggregate(AggregateDfcPolicy {
        sources: vec!["foo".to_string()],
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        constraint: "sum(reports.id) > 1".to_string(),
        description: None,
    })];

    let result = PassantPlanner::new().plan_query(&ir, &policies);
    assert!(!result.chosen.finalize_metadata.is_empty());
}

#[test]
fn planner_records_rewrite_error_in_explain_output() {
    let ir = parse_query_to_ir("SELECT id FROM bar EXCEPT SELECT id FROM foo")
        .expect("query should parse");
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
        Some("unsupported query form: EXCEPT with registered policies is non-monotonic")
    );
    assert_eq!(
        explanation.steps.last().map(|step| step.stage.as_str()),
        Some("fallback")
    );
}

#[test]
fn planner_records_source_set_rewrite_error_in_explain_output() {
    let ir = parse_query_to_ir("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id")
        .expect("query should parse");
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
    assert_eq!(
        explanation.chosen.rewrite_error.as_deref(),
        Some(
            "unsupported query form: outer join policy enforcement for nullable sources requires source-set annotations"
        )
    );
    assert!(explanation.scope.requires_source_set_annotations);
    assert_eq!(
        explanation.steps.last().map(|step| step.stage.as_str()),
        Some("fallback")
    );
}

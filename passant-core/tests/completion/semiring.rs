//! Full-Push and Partial-Push semiring rewrite completion tests.

use passant_core::{PolicyIr, Resolution, RewriteStrategy};

use crate::common::{assert_explain_strategy, plan_query, rewrite};

fn multi_source_sum_policy() -> PolicyIr {
    PolicyIr::Pgn {
        sources: vec!["foo".to_string(), "bar".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "sum(foo.amount) + sum(bar.amount) > 100".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }
}

#[test]
fn full_push_inner_join_decomposes_distributive_sum() {
    let policies = vec![multi_source_sum_policy()];
    assert_explain_strategy(
        "SELECT foo.id, bar.id FROM foo INNER JOIN bar ON foo.id = bar.id",
        &policies,
        RewriteStrategy::FullPush,
    );
    let sql = rewrite(
        "SELECT foo.id, bar.id FROM foo INNER JOIN bar ON foo.id = bar.id",
        &policies,
    );
    assert!(
        !sql.contains("SELECT sum("),
        "semiring rewrite should inline aggregates instead of scalar subqueries: {sql}"
    );
    assert!(sql.contains("foo.amount") && sql.contains("bar.amount"));
}

#[test]
fn full_push_nested_subquery_join_uses_semiring_not_scalar_fallback() {
    let policies = vec![PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "sum(foo.amount) > 100".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];
    let sql = rewrite(
        "SELECT s.total FROM (SELECT foo.id, sum(foo.amount) AS total FROM foo GROUP BY foo.id) AS s",
        &policies,
    );
    assert!(
        !sql.contains("(SELECT sum("),
        "expected semiring inline rewrite, got scalar fallback: {sql}"
    );
}

#[test]
fn full_push_left_join_preserves_nullable_side_semantics() {
    let policies = vec![PolicyIr::Pgn {
        sources: vec!["bar".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "sum(bar.amount) > 10".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];
    let result = plan_query(
        "SELECT foo.id, bar.amount FROM foo LEFT JOIN bar ON foo.id = bar.id",
        &policies,
    );
    assert_eq!(result.chosen.strategy, RewriteStrategy::FullPush);
    let sql = rewrite(
        "SELECT foo.id, bar.amount FROM foo LEFT JOIN bar ON foo.id = bar.id",
        &policies,
    );
    assert!(sql.contains("LEFT JOIN"));
}

#[test]
fn distributive_sum_decomposition_across_sources() {
    let policies = vec![multi_source_sum_policy()];
    let sql = rewrite(
        "SELECT foo.id FROM foo INNER JOIN bar ON foo.id = bar.id",
        &policies,
    );
    assert_eq!(
        sql,
        "SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id AND foo.amount + bar.amount > 100"
    );
}

#[test]
fn non_distributive_aggregate_keeps_partial_push_with_explicit_reason() {
    let policies = vec![PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "avg(foo.amount) > 100".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];
    let result = plan_query(
        "SELECT foo.id FROM foo INNER JOIN bar ON foo.id = bar.id",
        &policies,
    );
    let partial = result
        .candidates
        .iter()
        .find(|candidate| candidate.strategy == RewriteStrategy::PartialPush)
        .expect("partial push candidate should exist");
    assert!(
        partial
            .reasons
            .iter()
            .any(|reason| reason.contains("non-distributive")),
        "expected explicit non-distributive reason, got {:?}",
        partial.reasons
    );
}

#[test]
fn aggregation_query_full_push_inlines_having_semiring() {
    let policies = vec![PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "sum(foo.amount) > 100".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];
    let sql = rewrite(
        "SELECT foo.category, sum(foo.amount) FROM foo GROUP BY foo.category",
        &policies,
    );
    assert!(!sql.contains("WITH base_query AS ("));
    assert_eq!(
        sql,
        "SELECT foo.category, sum(foo.amount) FROM foo GROUP BY foo.category HAVING sum(foo.amount) > 100"
    );
}

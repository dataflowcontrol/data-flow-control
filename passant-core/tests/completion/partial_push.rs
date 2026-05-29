//! Partial-push rewrite completion tests (non-semiring policies only).

use passant_core::{PolicyIr, Resolution};

use crate::common::rewrite;

fn avg_policy(source: &str, constraint: &str) -> PolicyIr {
    PolicyIr::Pgn {
        sources: vec![source.to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: constraint.to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }
}

#[test]
fn partial_push_limit_aggregation_aliases_outer_projection() {
    let sql = rewrite(
        "SELECT o_orderkey, sum(l_quantity) FROM orders JOIN lineitem ON o_orderkey = l_orderkey GROUP BY o_orderkey ORDER BY o_orderkey LIMIT 10",
        &[avg_policy("lineitem", "avg(lineitem.l_quantity) >= 1")],
    );
    assert!(
        sql.contains("WITH base_query AS ("),
        "expected base_query CTE: {sql}"
    );
    assert!(
        sql.contains("policy_eval AS ("),
        "expected policy_eval CTE: {sql}"
    );
    assert!(
        sql.contains("cte AS ("),
        "expected limit boundary CTE: {sql}"
    );
    assert!(
        sql.contains("sum_l_quantity"),
        "expected aggregate alias: {sql}"
    );
    assert!(
        sql.contains("FROM cte WHERE"),
        "expected outer filter after limit: {sql}"
    );
}

#[test]
fn partial_push_aggregation_splits_base_and_policy_eval() {
    let sql = rewrite(
        "SELECT foo.category, sum(foo.amount) FROM foo GROUP BY foo.category",
        &[avg_policy("foo", "avg(foo.amount) > 100")],
    );
    assert!(sql.contains("WITH base_query AS ("));
    assert!(sql.contains("policy_eval AS ("));
    assert!(sql.contains("FROM base_query JOIN policy_eval"));
    assert!(sql.contains("avg(foo.amount) > 100"));
}

#[test]
fn partial_push_limit_scan_uses_cte_wrapper() {
    let sql = rewrite(
        "SELECT id FROM foo ORDER BY id LIMIT 1",
        &[avg_policy("foo", "avg(foo.id) > 1")],
    );
    assert!(sql.contains("WITH __passant_partial AS ("));
    assert!(sql.contains("FROM __passant_partial WHERE"));
}

use passant_core::{PolicyIr, Resolution};

use crate::common::{assert_rewrite, pgn_policy, rewrite};

#[test]
fn cross_join_applies_root_where_filter() {
    assert_rewrite(
        "SELECT foo.id FROM foo CROSS JOIN baz",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
        "SELECT foo.id FROM foo CROSS JOIN baz WHERE foo.id > 1",
    );
}

#[test]
fn select_distinct_scan_applies_where_filter() {
    assert_rewrite(
        "SELECT DISTINCT id FROM foo",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
        "SELECT DISTINCT id FROM foo WHERE foo.id > 1",
    );
}

#[test]
fn select_distinct_aggregation_applies_having_filter() {
    assert_rewrite(
        "SELECT DISTINCT COUNT(*) FROM foo",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
        "SELECT DISTINCT COUNT(*) FROM foo HAVING max(foo.id) > 1",
    );
}

#[test]
fn window_function_scan_preserves_over_clause() {
    let sql = rewrite(
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM foo",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
    );
    assert!(sql.contains("ROW_NUMBER() OVER"));
    assert!(sql.contains("foo.id > 1"));
}

#[test]
fn correlated_exists_subquery_combines_outer_where_filter() {
    let sql = rewrite(
        "SELECT id FROM foo WHERE EXISTS (SELECT 1 FROM baz WHERE baz.x = foo.id)",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
    );
    assert!(sql.contains("EXISTS"));
    assert!(sql.contains("foo.id > 1"));
}

#[test]
fn in_list_predicate_combines_with_policy_filter() {
    let sql = rewrite(
        "SELECT id FROM foo WHERE id IN (1, 2, 3)",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
    );
    assert!(sql.contains("IN (1, 2, 3)"));
    assert!(sql.contains("foo.id > 1"));
}

#[test]
fn approx_count_distinct_equality_rewrites_to_one() {
    assert_rewrite(
        "SELECT id FROM foo",
        &[PolicyIr::Pgn {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "approx_count_distinct(foo.id) = 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
        "SELECT id FROM foo WHERE 1 = 1",
    );
}

#[test]
fn nested_aggregation_constraint_uses_inner_expression() {
    let sql = rewrite(
        "SELECT id FROM foo",
        &[PolicyIr::Pgn {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "sum(max(foo.amount)) > 0".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
    );
    assert!(
        sql.contains("foo.amount") || sql.contains("sum("),
        "expected nested aggregation fallback in rewrite: {sql}"
    );
}

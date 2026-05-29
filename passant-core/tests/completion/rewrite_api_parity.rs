//! Rewrite API parity tests ported from legacy `sql_rewriter` behaviors.

use passant_core::{PolicyIr, Resolution, TableCatalog};

use crate::common::{assert_rewrite, pgn_policy, pgn_policy_kill, rewrite, rewrite_with_catalog};

#[test]
fn scan_count_if_transforms_to_case_when() {
    let policy = PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "COUNT_IF(foo.id > 2) > 0".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    assert_rewrite(
        "SELECT id FROM foo",
        &[policy],
        "SELECT id FROM foo WHERE CASE WHEN foo.id > 2 THEN 1 ELSE 0 END > 0",
    );
}

#[test]
fn scan_array_agg_non_distributive_uses_partial_push() {
    let sql = rewrite(
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
            constraint: "array_agg(foo.id) = [foo.id]".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
    );
    assert!(sql.contains("WITH base_query AS ("));
    assert!(sql.contains("policy_eval AS ("));
}

#[test]
fn aggregation_kill_wraps_having_clause() {
    let sql = rewrite(
        "SELECT category, sum(amount) FROM foo GROUP BY category",
        &[pgn_policy_kill(&["foo"], "sum(foo.amount) > 100")],
    );
    assert!(sql.contains("passant_kill"));
    assert!(sql.contains("t1 AS"));
    assert!(sql.contains("GROUP BY category"));
}

#[test]
fn scan_count_distinct_equality_uses_global_cardinality_subquery() {
    let sql = rewrite(
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
            constraint: "count(distinct foo.id) = 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
    );
    assert!(
        sql.contains("count(DISTINCT foo.id) = 1"),
        "expected global COUNT(DISTINCT) predicate on scan: {sql}"
    );
}

#[test]
fn scan_avg_non_distributive_uses_partial_push() {
    let sql = rewrite(
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
            constraint: "avg(foo.amount) > 100".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
    );
    assert!(sql.contains("WITH base_query AS ("));
    assert!(sql.contains("policy_eval AS ("));
    assert!(sql.contains("avg(foo.amount) > 100"));
}

#[test]
fn scan_min_max_preserve_full_expression() {
    assert_rewrite(
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
            constraint: "min(foo.amount + 1) > 0".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
        "SELECT id FROM foo WHERE foo.amount + 1 > 0",
    );
}

#[test]
fn dimension_table_constraint_references_external_context() {
    assert_rewrite(
        "SELECT foo.id FROM foo JOIN regions ON foo.region_id = regions.id",
        &[PolicyIr::Pgn {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimension_tables: vec!["regions".to_string()],
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "max(foo.id) > 1 AND regions.code = 'US'".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
        "SELECT foo.id FROM foo JOIN regions ON foo.region_id = regions.id WHERE foo.id > 1 AND regions.code = 'US'",
    );
}

#[test]
fn insert_without_column_list_expands_from_catalog() {
    let mut catalog = TableCatalog::new();
    catalog.register_table("reports", vec!["id".to_string(), "amount".to_string()]);
    let policy = PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    let actual = rewrite_with_catalog(
        "INSERT INTO reports SELECT id, amount FROM foo",
        &[policy],
        catalog,
    );
    pretty_assertions::assert_eq!(
        actual,
        "INSERT INTO reports (id, amount) SELECT id, amount FROM foo WHERE foo.id > 1"
    );
}

#[test]
fn merge_statement_rewrite_supported() {
    let sql = rewrite(
        "MERGE INTO reports USING foo ON reports.id = foo.id WHEN MATCHED THEN UPDATE SET amount = foo.amount",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
    );
    assert!(sql.contains("MERGE INTO reports"));
    assert!(sql.contains("foo.id > 1"));
}

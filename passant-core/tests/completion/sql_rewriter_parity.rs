//! Ports of high-value `sql_rewriter` behaviors not yet covered in Rust.

use passant_core::{PolicyIr, Resolution, TableCatalog};

use crate::common::{
    assert_rewrite, dfc_policy, dfc_policy_invalidate, dfc_policy_kill, rewrite,
    rewrite_with_catalog,
};

#[test]
fn scan_count_if_transforms_to_case_when() {
    let policy = PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
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
fn scan_array_agg_transforms_to_single_element_array() {
    let policy = PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "array_agg(foo.id) = [foo.id]".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    assert_rewrite(
        "SELECT id FROM foo",
        &[policy],
        "SELECT id FROM foo WHERE [foo.id] = [foo.id]",
    );
}

#[test]
fn aggregation_kill_wraps_having_clause() {
    assert_rewrite(
        "SELECT category, sum(amount) FROM foo GROUP BY category",
        &[dfc_policy_kill(&["foo"], "sum(foo.amount) > 100")],
        "SELECT category, sum(amount) FROM foo GROUP BY category HAVING CASE WHEN sum(foo.amount) > 100 THEN (foo.category = foo.category) OR kill() ELSE true END",
    );
}

#[test]
fn aggregation_invalidate_adds_valid_to_grouped_select() {
    assert_rewrite(
        "SELECT category, sum(amount) FROM foo GROUP BY category",
        &[dfc_policy_invalidate(&["foo"], "sum(foo.amount) > 100")],
        "SELECT category, sum(amount), sum(foo.amount) > 100 AS valid FROM foo GROUP BY category",
    );
}

#[test]
fn scan_count_distinct_equality_expands_to_row_predicate() {
    assert_rewrite(
        "SELECT id FROM foo",
        &[PolicyIr::CompatDfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "count(distinct foo.id) = 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
        "SELECT id FROM foo WHERE foo.id IS NOT NULL",
    );
}

#[test]
fn scan_avg_non_distributive_uses_scalar_subquery_fallback() {
    let sql = rewrite(
        "SELECT id FROM foo",
        &[PolicyIr::CompatDfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "avg(foo.amount) > 100".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
    );
    assert!(
        sql.contains("(SELECT avg("),
        "expected scalar subquery fallback for avg(): {sql}"
    );
}

#[test]
fn scan_min_max_preserve_full_expression() {
    assert_rewrite(
        "SELECT id FROM foo",
        &[PolicyIr::CompatDfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "min(foo.amount + 1) > 0".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
        "SELECT id FROM foo WHERE foo.amount + 1 > 0",
    );
}

#[test]
fn multi_policy_invalidate_combines_valid_columns() {
    assert_rewrite(
        "SELECT id FROM foo",
        &[
            dfc_policy_invalidate(&["foo"], "max(foo.id) > 1"),
            dfc_policy_invalidate(&["foo"], "max(foo.amount) > 10"),
        ],
        "SELECT id, foo.id > 1 AND foo.amount > 10 AS valid FROM foo",
    );
}

#[test]
fn dimension_table_constraint_references_external_context() {
    assert_rewrite(
        "SELECT foo.id FROM foo JOIN regions ON foo.region_id = regions.id",
        &[PolicyIr::CompatDfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: vec!["regions.code".to_string()],
            sink: None,
            sink_alias: None,
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
    let policy = PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Invalidate,
        description: None,
    };
    let actual = rewrite_with_catalog(
        "INSERT INTO reports SELECT id, amount FROM foo",
        &[policy],
        catalog,
    );
    pretty_assertions::assert_eq!(
        actual,
        "INSERT INTO reports (id, amount, valid) SELECT id, amount, foo.id > 1 AS valid FROM foo"
    );
}

#[test]
fn merge_statement_rewrite_supported() {
    let sql = rewrite(
        "MERGE INTO reports USING foo ON reports.id = foo.id WHEN MATCHED THEN UPDATE SET amount = foo.amount",
        &[dfc_policy(&["foo"], "max(foo.id) > 1")],
    );
    assert!(sql.contains("MERGE INTO reports"));
    assert!(sql.contains("foo.id > 1"));
}

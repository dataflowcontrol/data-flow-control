use passant_core::{PassantRewriter, PolicyIr, Resolution, TableCatalog};

use crate::common::{assert_rewrite, pgn_policy, rewrite_with_catalog};

#[test]
fn insert_values_statement_is_not_rewritten() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(pgn_policy(&["foo"], "max(foo.id) > 1"));
    let sql = rewriter
        .rewrite("INSERT INTO foo VALUES (1), (2)")
        .expect("INSERT VALUES should pass through");
    assert_eq!(sql, "INSERT INTO foo VALUES (1), (2)");
}

fn reports_catalog() -> TableCatalog {
    let mut catalog = TableCatalog::new();
    catalog.register_table("reports", vec!["id".into(), "status".into()]);
    catalog
}

#[test]
fn sink_only_remove_policy_on_insert_select() {
    let sql = rewrite_with_catalog(
        "INSERT INTO reports SELECT id, status FROM foo",
        &[PolicyIr::Pgn {
            sources: vec![],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: Some("reports".to_string()),
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "reports.status = 'approved'".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
        reports_catalog(),
    );
    assert!(
        sql.contains("CASE WHEN") || sql.contains("status = 'approved'"),
        "expected sink-only remove filter on INSERT SELECT: {sql}"
    );
}

#[test]
fn sink_only_kill_policy_on_insert_select() {
    let sql = rewrite_with_catalog(
        "INSERT INTO reports SELECT id, status FROM foo",
        &[PolicyIr::Pgn {
            sources: vec![],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: Some("reports".to_string()),
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "reports.status = 'approved'".to_string(),
            on_fail: Resolution::Kill,
            description: None,
        }],
        reports_catalog(),
    );
    assert!(
        sql.to_ascii_uppercase().contains("KILL"),
        "expected sink-only kill wrap on INSERT SELECT: {sql}"
    );
}

#[test]
fn insert_select_kill_order_by_remapped() {
    let sql = rewrite_with_catalog(
        "INSERT INTO reports SELECT id FROM receipts ORDER BY Receipts.id",
        &[PolicyIr::Pgn {
            sources: vec![],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: Some("reports".to_string()),
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "reports.id > 0".to_string(),
            on_fail: Resolution::Kill,
            description: None,
        }],
        reports_catalog(),
    );
    assert!(sql.contains("passant_kill"), "expected kill wrap: {sql}");
    assert!(
        sql.contains("ORDER BY id"),
        "expected unqualified ORDER BY: {sql}"
    );
    assert!(
        !sql.contains("ORDER BY Receipts."),
        "qualified ORDER BY should be remapped: {sql}"
    );
}

#[test]
fn multiple_kill_policies_on_insert_select() {
    let sql = rewrite_with_catalog(
        "INSERT INTO reports SELECT id, status FROM foo",
        &[
            PolicyIr::Pgn {
                sources: vec![],
                required_sources: Vec::new(),
                dimension_tables: Vec::new(),
                dimension_aliases: std::collections::HashMap::new(),
                dimension_queries: std::collections::HashMap::new(),
                sink: Some("reports".to_string()),
                sink_alias: None,
                source_aliases: std::collections::HashMap::new(),
                constraint: "reports.status = 'approved'".to_string(),
                on_fail: Resolution::Kill,
                description: None,
            },
            PolicyIr::Pgn {
                sources: vec![],
                required_sources: Vec::new(),
                dimension_tables: Vec::new(),
                dimension_aliases: std::collections::HashMap::new(),
                dimension_queries: std::collections::HashMap::new(),
                sink: Some("reports".to_string()),
                sink_alias: None,
                source_aliases: std::collections::HashMap::new(),
                constraint: "reports.id > 0".to_string(),
                on_fail: Resolution::Kill,
                description: None,
            },
        ],
        reports_catalog(),
    );
    assert!(sql.contains("passant_kill"), "expected kill wrap: {sql}");
    assert_eq!(
        sql.matches("t1 AS").count(),
        1,
        "expected single t1 CTE for fused kill policies: {sql}"
    );
    assert_eq!(
        sql.matches("passant_kill()").count(),
        1,
        "expected single passant_kill call: {sql}"
    );
}

#[test]
fn insert_with_cte_and_subquery_recurses_into_source() {
    assert_rewrite(
        "WITH src AS (SELECT id FROM foo) INSERT INTO bar SELECT id FROM src",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
        "WITH src AS (SELECT id FROM foo WHERE foo.id > 1) INSERT INTO bar SELECT id FROM src",
    );
}

#[test]
fn nested_cte_recurses_policy_into_inner_cte() {
    let sql = rewrite_with_catalog(
        "WITH outer_cte AS (WITH inner_cte AS (SELECT id FROM foo) SELECT id FROM inner_cte) \
SELECT id FROM outer_cte",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
        TableCatalog::new(),
    );
    assert!(
        sql.contains("foo.id > 1"),
        "expected policy filter inside nested CTE: {sql}"
    );
}

#[test]
fn multiple_ctes_with_join_recurses_into_both_branches() {
    let sql = rewrite_with_catalog(
        "WITH a AS (SELECT id FROM foo), b AS (SELECT x FROM baz) \
SELECT a.id, b.x FROM a JOIN b ON a.id = b.x",
        &[
            pgn_policy(&["foo"], "max(foo.id) > 1"),
            pgn_policy(&["baz"], "max(baz.x) > 5"),
        ],
        TableCatalog::new(),
    );
    assert!(sql.contains("foo.id > 1"));
    assert!(sql.contains("baz.x > 5"));
}

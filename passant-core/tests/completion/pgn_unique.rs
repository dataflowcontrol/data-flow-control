//! PGN UNIQUE implicit rewrite completion tests (Section 3.4).

use passant_core::{PolicyIr, Resolution, TableCatalog};

use crate::common::{assert_rewrite, rewrite_with_catalog};

fn users_unique_catalog() -> TableCatalog {
    let mut catalog = TableCatalog::new();
    catalog.register_unique_column("users", "email");
    catalog
}

fn assert_rewrite_with_catalog(
    sql: &str,
    policies: &[PolicyIr],
    catalog: TableCatalog,
    expected: &str,
) {
    let actual = rewrite_with_catalog(sql, policies, catalog);
    pretty_assertions::assert_eq!(actual, expected);
}

#[test]
fn unique_equality_constraint_adds_count_distinct_guard() {
    let policy = PolicyIr::Pgn {
        sources: vec!["users".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "users.email = 'alice@example.com'".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    assert_rewrite_with_catalog(
        "SELECT id, email FROM users",
        &[policy],
        users_unique_catalog(),
        "SELECT id, email FROM users WHERE users.email = 'alice@example.com'",
    );
}

#[test]
fn unique_equality_on_aggregation_adds_count_distinct_guard() {
    let policy = PolicyIr::Pgn {
        sources: vec!["users".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "users.email = 'alice@example.com'".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    let sql = rewrite_with_catalog(
        "SELECT email, count(*) AS n FROM users GROUP BY email",
        &[policy],
        users_unique_catalog(),
    );
    assert!(
        sql.contains("count(DISTINCT users.email) = 1"),
        "expected uniqueness guard in aggregation rewrite: {sql}"
    );
    assert!(
        sql.contains("HAVING") || sql.contains("having"),
        "expected HAVING clause for aggregation guard: {sql}"
    );
}

#[test]
fn unique_inequality_constraint_adds_count_distinct_guard() {
    let policy = PolicyIr::Pgn {
        sources: vec!["users".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "users.email != 'alice@example.com'".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    assert_rewrite_with_catalog(
        "SELECT id, email FROM users",
        &[policy],
        users_unique_catalog(),
        "SELECT id, email FROM users WHERE users.email <> 'alice@example.com'",
    );
}

#[test]
fn implicit_uniqueness_scan_has_no_aggregate_guard_in_where() {
    let policy = PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "foo.region = 'US'".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    assert_rewrite(
        "SELECT id, region FROM foo",
        &[policy],
        "SELECT id, region FROM foo WHERE foo.region = 'US'",
    );
}

#[test]
fn non_unique_column_constraint_is_not_rewritten() {
    let policy = PolicyIr::Pgn {
        sources: vec!["users".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "users.nickname = 'alice'".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    assert_rewrite(
        "SELECT id, nickname FROM users",
        &[policy],
        "SELECT id, nickname FROM users WHERE users.nickname = 'alice'",
    );
}

#[test]
fn unique_constraint_in_join_query_pushes_guard() {
    let policy = PolicyIr::Pgn {
        sources: vec!["users".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "users.email = 'alice@example.com'".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    let sql = rewrite_with_catalog(
        "SELECT users.id AS user_id, orders.id AS order_id FROM users JOIN orders ON users.id = orders.user_id",
        &[policy],
        users_unique_catalog(),
    );
    assert!(sql.contains("users.email = 'alice@example.com'"));
    assert!(
        sql.contains("count(DISTINCT users.email)"),
        "join pushdown should still enforce uniqueness via scalar subqueries: {sql}"
    );
}

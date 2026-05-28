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
    let policy = PolicyIr::Dfc {
        sources: vec!["users".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "users.email = 'alice@example.com'".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    assert_rewrite_with_catalog(
        "SELECT id, email FROM users",
        &[policy],
        users_unique_catalog(),
        "SELECT id, email FROM users WHERE count(DISTINCT users.email) = 1 AND users.email = 'alice@example.com'",
    );
}

#[test]
fn unique_inequality_constraint_adds_count_distinct_guard() {
    let policy = PolicyIr::Dfc {
        sources: vec!["users".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "users.email != 'alice@example.com'".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    assert_rewrite_with_catalog(
        "SELECT id, email FROM users",
        &[policy],
        users_unique_catalog(),
        "SELECT id, email FROM users WHERE count(DISTINCT users.email) = 1 AND users.email <> 'alice@example.com'",
    );
}

#[test]
fn non_unique_column_constraint_is_not_rewritten() {
    let policy = PolicyIr::Dfc {
        sources: vec!["users".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
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
    let policy = PolicyIr::Dfc {
        sources: vec!["users".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "users.email = 'alice@example.com'".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    assert_rewrite_with_catalog(
        "SELECT users.id, orders.id FROM users JOIN orders ON users.id = orders.user_id",
        &[policy],
        users_unique_catalog(),
        "SELECT users.id, orders.id FROM users JOIN orders ON users.id = orders.user_id WHERE count(DISTINCT users.email) = 1 AND users.email = 'alice@example.com'",
    );
}

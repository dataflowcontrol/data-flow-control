//! PGN UNIQUE implicit rewrite completion tests (Section 3.4).

use passant_core::{PolicyIr, Resolution};

use crate::common::assert_rewrite;

#[test]
#[ignore = "completion: pgn_unique"]
fn unique_equality_constraint_adds_count_distinct_guard() {
    let policy = PolicyIr::CompatDfc {
        sources: vec!["users".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "users.email = 'alice@example.com'".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    assert_rewrite(
        "SELECT id, email FROM users",
        &[policy],
        "SELECT id, email FROM users WHERE count(distinct users.email) = 1 AND users.email = 'alice@example.com'",
    );
}

#[test]
#[ignore = "completion: pgn_unique"]
fn unique_inequality_constraint_adds_count_distinct_guard() {
    let policy = PolicyIr::CompatDfc {
        sources: vec!["users".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "users.email != 'alice@example.com'".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    assert_rewrite(
        "SELECT id, email FROM users",
        &[policy],
        "SELECT id, email FROM users WHERE count(distinct users.email) = 1 AND users.email != 'alice@example.com'",
    );
}

#[test]
#[ignore = "completion: pgn_unique"]
fn non_unique_column_constraint_is_not_rewritten() {
    let policy = PolicyIr::CompatDfc {
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
#[ignore = "completion: pgn_unique"]
fn unique_constraint_in_join_query_pushes_guard() {
    let policy = PolicyIr::CompatDfc {
        sources: vec!["users".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "users.email = 'alice@example.com'".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    assert_rewrite(
        "SELECT users.id, orders.id FROM users JOIN orders ON users.id = orders.user_id",
        &[policy],
        "SELECT users.id, orders.id FROM users JOIN orders ON users.id = orders.user_id WHERE count(distinct users.email) = 1 AND users.email = 'alice@example.com'",
    );
}

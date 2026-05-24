//! Semantic oracle: compare rewritten execution to naive row-level enforcement.

use passant_core::{PolicyIr, Resolution};

use crate::common::dfc_policy;
use crate::duckdb::TestDb;

#[test]
fn full_push_remove_matches_naive_row_filter_oracle() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER, amount INTEGER)");
    db.exec("INSERT INTO foo VALUES (1, 10), (2, 20), (3, 5)");
    db.register_policy(dfc_policy(&["foo"], "max(foo.id) > 1"));

    let rewritten = db
        .rewrite("SELECT id FROM foo ORDER BY id")
        .expect("rewrite");
    let actual = db.fetchall_i64(&rewritten);
    let oracle = db.fetchall_i64("SELECT id FROM foo WHERE foo.id > 1 ORDER BY id");

    assert_eq!(actual, oracle);
    assert_eq!(actual, vec![2, 3]);
}

#[test]
fn partial_push_remove_preserves_rows_when_policy_passes() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER)");
    db.exec("INSERT INTO foo VALUES (1), (3)");
    db.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "avg(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let rewritten = db
        .rewrite("SELECT id FROM foo ORDER BY id")
        .expect("rewrite");
    let actual = db.fetchall_i64(&rewritten);

    assert_eq!(actual, vec![1, 3]);
}

#[test]
fn partial_push_remove_filters_rows_when_aggregate_policy_fails() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER)");
    db.exec("INSERT INTO foo VALUES (1), (2), (3)");
    db.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "avg(foo.id) > 2".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let rewritten = db
        .rewrite("SELECT id FROM foo ORDER BY id")
        .expect("rewrite");
    let actual = db.fetchall_i64(&rewritten);

    assert_eq!(actual, vec![]);
}

#[test]
fn outer_join_cross_source_remove_execution_matches_naive_filter() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE bar (id INTEGER)");
    db.exec("CREATE TABLE foo (id INTEGER)");
    db.exec("INSERT INTO bar VALUES (1), (3)");
    db.exec("INSERT INTO foo VALUES (2), (4)");
    db.register_policy(PolicyIr::CompatDfc {
        sources: vec!["bar".to_string(), "foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(bar.id) > max(foo.id)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let rewritten = db
        .rewrite("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id ORDER BY bar.id")
        .expect("rewrite");
    assert_eq!(
        rewritten,
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id WHERE bar.id > foo.id ORDER BY bar.id"
    );
    assert_eq!(
        db.fetchall_i64(&rewritten),
        db.fetchall_i64(
            "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id WHERE bar.id > foo.id ORDER BY bar.id",
        ),
    );
}

#[test]
fn full_push_remove_with_limit_matches_naive_oracle() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER)");
    db.exec("INSERT INTO foo VALUES (1), (2), (3), (4)");
    db.register_policy(dfc_policy(&["foo"], "max(foo.id) > 1"));

    let rewritten = db
        .rewrite("SELECT id FROM foo ORDER BY id LIMIT 2")
        .expect("rewrite");
    let actual = db.fetchall_i64(&rewritten);
    let oracle = db.fetchall_i64("SELECT id FROM foo WHERE foo.id > 1 ORDER BY id LIMIT 2");
    assert_eq!(actual, oracle);
    assert_eq!(actual, vec![2, 3]);
}

#[test]
fn union_all_remove_matches_naive_oracle() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER)");
    db.exec("CREATE TABLE bar (id INTEGER)");
    db.exec("INSERT INTO foo VALUES (1), (2), (3)");
    db.exec("INSERT INTO bar VALUES (10), (20)");
    db.register_policy(dfc_policy(&["foo"], "max(foo.id) > 1"));

    let query = "SELECT id FROM foo UNION ALL SELECT id FROM bar ORDER BY id";
    let rewritten = db.rewrite(query).expect("rewrite");
    let actual = db.fetchall_i64(&rewritten);
    let oracle = db.fetchall_i64(
        "SELECT id FROM foo WHERE foo.id > 1 UNION ALL SELECT id FROM bar ORDER BY id",
    );
    assert_eq!(actual, oracle);
    assert_eq!(actual, vec![2, 3, 10, 20]);
}

#[test]
fn correlated_exists_remove_matches_naive_oracle() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER)");
    db.exec("CREATE TABLE baz (x INTEGER)");
    db.exec("INSERT INTO foo VALUES (1), (2), (3), (4)");
    db.exec("INSERT INTO baz VALUES (2), (3), (5)");
    db.register_policy(dfc_policy(&["foo"], "max(foo.id) > 1"));

    let query =
        "SELECT id FROM foo WHERE EXISTS (SELECT 1 FROM baz WHERE baz.x = foo.id) ORDER BY id";
    let rewritten = db.rewrite(query).expect("rewrite");
    let actual = db.fetchall_i64(&rewritten);
    let oracle = db.fetchall_i64(
        "SELECT id FROM foo WHERE foo.id > 1 AND EXISTS (SELECT 1 FROM baz WHERE baz.x = foo.id) ORDER BY id",
    );
    assert_eq!(actual, oracle);
    assert_eq!(actual, vec![2, 3]);
}

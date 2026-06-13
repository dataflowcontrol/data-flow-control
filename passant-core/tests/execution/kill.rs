use crate::common::pgn_policy_with;
use crate::duckdb::TestDb;
use passant_core::{PolicyIr, Resolution};

#[test]
fn kill_aborts_query_when_constraint_fails() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER)");
    db.exec("INSERT INTO foo VALUES (1)");
    db.register_policy(pgn_policy_with(
        &["foo"],
        "max(foo.id) > 10",
        Resolution::Kill,
    ));

    let message = db.run_rewritten_expect_error("SELECT id FROM foo");
    assert!(message.contains("KILLing due to dfc policy violation"));
}

#[test]
fn kill_allows_query_when_constraint_passes() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER)");
    db.exec("INSERT INTO foo VALUES (20)");
    db.register_policy(pgn_policy_with(
        &["foo"],
        "max(foo.id) > 10",
        Resolution::Kill,
    ));

    assert_eq!(db.rewrite_and_fetch_i64("SELECT id FROM foo"), vec![20]);
}

#[test]
fn kill_rewrite_emits_kill_call_in_sql() {
    let mut db = TestDb::new();
    db.register_policy(pgn_policy_with(
        &["foo"],
        "max(foo.id) > 10",
        Resolution::Kill,
    ));

    let rewritten = db
        .rewrite("SELECT id FROM foo")
        .expect("rewrite should succeed");
    assert!(rewritten.contains("passant_kill"));
    assert!(rewritten.contains("t1 AS"));
}

#[test]
fn insert_select_kill_order_by_executes() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE receipts (id INTEGER)");
    db.exec("CREATE TABLE reports (id INTEGER)");
    db.exec("INSERT INTO receipts VALUES (2), (1)");
    db.register_policy(PolicyIr::Pgn {
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
    });

    db.rewrite_exec(
        "INSERT INTO reports (id) SELECT receipts.id FROM receipts ORDER BY Receipts.id",
    );
    assert_eq!(
        db.fetchall_i64("SELECT id FROM reports ORDER BY id"),
        vec![1, 2]
    );
}

#[test]
fn multiple_kill_policies_abort_on_any_failure() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER, status VARCHAR)");
    db.exec("CREATE TABLE reports (id INTEGER, status VARCHAR)");
    db.exec("INSERT INTO foo VALUES (1, 'draft')");
    db.register_policy(PolicyIr::Pgn {
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
    });
    db.register_policy(PolicyIr::Pgn {
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
    });

    let message = db.run_rewritten_expect_error(
        "INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo",
    );
    assert!(message.contains("KILLing due to dfc policy violation"));
}

use crate::common::pgn_policy_with;
use crate::duckdb::TestDb;
use passant_core::Resolution;

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

use crate::common::dfc_policy_with;
use crate::duckdb::TestDb;
use passant_core::Resolution;

#[test]
fn kill_aborts_query_when_constraint_fails() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER)");
    db.exec("INSERT INTO foo VALUES (1)");
    db.register_policy(dfc_policy_with(
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
    db.register_policy(dfc_policy_with(
        &["foo"],
        "max(foo.id) > 10",
        Resolution::Kill,
    ));

    assert_eq!(db.rewrite_and_fetch_i64("SELECT id FROM foo"), vec![20]);
}

#[test]
fn kill_rewrite_emits_kill_call_in_sql() {
    let mut db = TestDb::new();
    db.register_policy(dfc_policy_with(
        &["foo"],
        "max(foo.id) > 10",
        Resolution::Kill,
    ));

    let rewritten = db
        .rewrite("SELECT id FROM foo")
        .expect("rewrite should succeed");
    assert_eq!(
        rewritten,
        "SELECT id FROM foo WHERE (foo.id > 10) OR kill()"
    );
}

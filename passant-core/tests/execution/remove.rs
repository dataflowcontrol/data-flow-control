use crate::common::dfc_policy;
use crate::duckdb::TestDb;

#[test]
fn remove_filters_violating_rows_from_scan() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER)");
    db.exec("INSERT INTO foo VALUES (1), (2), (3)");
    db.register_policy(dfc_policy(&["foo"], "max(foo.id) > 1"));

    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT id FROM foo ORDER BY id"),
        vec![2, 3]
    );
}

#[test]
fn remove_filters_violating_groups_from_aggregation() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (category VARCHAR, amount INTEGER)");
    db.exec("INSERT INTO foo VALUES ('a', 1), ('a', 5), ('b', 10)");
    db.register_policy(dfc_policy(&["foo"], "max(foo.amount) > 6"));

    let rewritten = db
        .rewrite(
            "SELECT category, sum(amount) AS total FROM foo GROUP BY category ORDER BY category",
        )
        .expect("aggregation should rewrite");
    assert_eq!(
        db.fetchall_i64(&format!(
            "SELECT total FROM ({rewritten}) ORDER BY category"
        )),
        vec![10]
    );
}

#[test]
fn policy_not_applied_when_source_missing_from_query() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER)");
    db.exec("CREATE TABLE bar (id INTEGER)");
    db.exec("INSERT INTO bar VALUES (1), (2)");
    db.register_policy(dfc_policy(&["foo"], "max(foo.id) > 10"));

    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT id FROM bar ORDER BY id"),
        vec![1, 2]
    );
}

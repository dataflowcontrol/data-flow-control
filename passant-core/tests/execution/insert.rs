use crate::common::dfc_policy_sink;
use crate::duckdb::TestDb;
use passant_core::{PolicyIr, Resolution};

#[test]
fn insert_remove_filters_violating_sink_rows() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER, status VARCHAR)");
    db.exec("CREATE TABLE reports (id INTEGER, status VARCHAR)");
    db.exec("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')");
    db.register_policy(dfc_policy_sink(
        &["foo"],
        "reports",
        "reports.status = 'approved' AND max(foo.id) > 1",
    ));

    db.rewrite_exec("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo");
    assert_eq!(
        db.fetchall_i64("SELECT id FROM reports ORDER BY id"),
        vec![2]
    );
}

#[test]
fn required_source_fail_closed_on_insert() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE other (id INTEGER)");
    db.exec("CREATE TABLE reports (id INTEGER)");
    db.exec("INSERT INTO other VALUES (1)");
    db.register_policy(PolicyIr::Dfc {
        sources: vec!["receipts".to_string()],
        required_sources: vec!["receipts".to_string()],
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "reports.id > 0 AND max(receipts.id) > 0".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    db.rewrite_exec("INSERT INTO reports (id) SELECT other.id FROM other");
    assert_eq!(db.fetchall_i64("SELECT COUNT(*) FROM reports"), vec![0]);
}

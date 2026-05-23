use crate::common::dfc_policy_with;
use crate::duckdb::TestDb;
use passant_core::{PolicyIr, Resolution};

#[test]
fn invalidate_adds_valid_column_to_scan() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER)");
    db.exec("INSERT INTO foo VALUES (1), (2)");
    db.register_policy(dfc_policy_with(
        &["foo"],
        "max(foo.id) > 1",
        Resolution::Invalidate,
    ));

    let rewritten = db
        .rewrite("SELECT id FROM foo ORDER BY id")
        .expect("rewrite");
    assert_eq!(
        db.fetchall_bool(&format!("SELECT valid FROM ({rewritten}) ORDER BY id")),
        vec![false, true]
    );
}

#[test]
fn invalidate_message_adds_message_column() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER, invalid_string VARCHAR)");
    db.exec("INSERT INTO foo VALUES (1, NULL), (2, NULL)");
    db.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::InvalidateMessage,
        description: Some("too small".to_string()),
    });

    let rewritten = db
        .rewrite("SELECT id FROM foo WHERE id = 1")
        .expect("rewrite");
    assert_eq!(
        db.fetchall_strings(&format!(
            "SELECT COALESCE(invalid_string, '') FROM ({rewritten})"
        )),
        vec!["too small".to_string()]
    );
}

#[test]
fn llm_resolution_filters_rows_with_default_resolver() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER)");
    db.exec("INSERT INTO foo VALUES (1), (2)");
    db.register_policy(dfc_policy_with(
        &["foo"],
        "max(foo.id) > 1",
        Resolution::Llm,
    ));

    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT id FROM foo ORDER BY id"),
        vec![2]
    );
}

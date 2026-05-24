use crate::common::aggregate_policy;
use crate::duckdb::TestDb;
use passant_core::{AggregateDfcPolicy, PolicyIr};

#[test]
fn finalize_aggregate_policies_invalidates_sink_rows() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE reports (id INTEGER, valid BOOLEAN)");
    db.exec("INSERT INTO reports VALUES (1, TRUE), (2, TRUE)");
    db.register_policy(aggregate_policy(&[], "reports", "sum(reports.id) > 10"));

    db.finalize_aggregate_policies("reports");
    assert_eq!(
        db.fetchall_bool("SELECT valid FROM reports ORDER BY id"),
        vec![false, false]
    );
}

#[test]
fn aggregate_temp_columns_and_finalize_invalidate_violating_sink() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (amount INTEGER)");
    db.exec("CREATE TABLE reports (total INTEGER, valid BOOLEAN, __passant_agg_0 INTEGER, __passant_agg_1 INTEGER)");
    db.exec("INSERT INTO foo VALUES (1), (2)");
    db.register_policy(aggregate_policy(
        &["foo"],
        "reports",
        "sum(foo.amount) > sum(reports.total)",
    ));

    db.rewrite_exec("INSERT INTO reports (total) SELECT foo.amount FROM foo");
    db.finalize_aggregate_policies("reports");
    assert_eq!(
        db.fetchall_bool("SELECT valid FROM reports ORDER BY total"),
        vec![false, false]
    );
}

#[test]
fn dimensioned_finalize_invalidates_matching_groups_only() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE reports (region VARCHAR, total INTEGER, valid BOOLEAN)");
    db.exec(
        "INSERT INTO reports VALUES ('east', 40, TRUE), ('east', 70, TRUE), ('west', 20, TRUE), ('west', 30, TRUE)",
    );
    db.register_policy(PolicyIr::CompatAggregate(AggregateDfcPolicy {
        sources: Vec::new(),
        dimensions: vec!["reports.region".to_string()],
        sink: Some("reports".to_string()),
        constraint: "sum(reports.total) > 100".to_string(),
        description: None,
    }));

    db.finalize_aggregate_policies("reports");
    assert_eq!(
        db.fetchall_bool("SELECT valid FROM reports ORDER BY region, total"),
        vec![true, true, false, false]
    );
}

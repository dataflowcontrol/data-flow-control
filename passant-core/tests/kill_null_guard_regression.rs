//! FALSE must reach the relation-level abort; NULL retains legacy filtering.
#[path = "common/duckdb.rs"]
mod fixture;

use passant_core::sql::{passant_kill_tuple_filter, qualified_column};
use sqlparser::ast::{BinaryOperator, Expr};

#[test]
fn kill_tuple_guard_filters_only_null_not_false_candidates() {
    let guard = passant_kill_tuple_filter("candidates", "pass");
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::And,
        right,
    } = &guard
    else {
        panic!("expected aggregate abort guard AND null filter: {guard}");
    };
    assert!(matches!(left.as_ref(), Expr::Subquery(_)), "{guard}");
    assert_eq!(
        right.as_ref(),
        &Expr::IsNotNull(Box::new(qualified_column("candidates", "pass")))
    );
    let abort = left.to_string();
    assert!(
        abort.contains("max(CASE WHEN NOT candidates.pass THEN 1 ELSE 0 END)"),
        "{abort}"
    );
    assert_eq!(abort.matches("passant_kill()").count(), 1);
}

#[test]
fn constant_false_kill_aborts_insert_without_changing_existing_rows() {
    let db = fixture::TestDb::new();
    db.exec("CREATE TABLE target(id INTEGER); INSERT INTO target VALUES (99)");
    let guard = passant_kill_tuple_filter("candidates", "pass");
    let sql = format!(
        "INSERT INTO target WITH candidates AS (SELECT false AS pass) SELECT 7 FROM candidates WHERE {guard}"
    );
    let error = db
        .conn
        .execute_batch(&sql)
        .expect_err("constant FALSE must not optimize away KILL");
    assert!(
        error
            .to_string()
            .contains("KILLing due to dfc policy violation"),
        "{error}"
    );
    assert_eq!(db.fetchall_i64("SELECT id FROM target"), vec![99]);
}

#[test]
fn kill_null_candidates_keep_legacy_null_filtering() {
    let db = fixture::TestDb::new();
    let guard = passant_kill_tuple_filter("candidates", "pass");
    let sql = format!(
        "WITH candidates AS (SELECT NULL::BOOLEAN AS pass) SELECT 7 FROM candidates WHERE {guard}"
    );
    assert!(db.fetchall_i64(&sql).is_empty());
    let sql =
        format!("WITH candidates AS (SELECT true AS pass) SELECT 7 FROM candidates WHERE {guard}");
    assert_eq!(db.fetchall_i64(&sql), vec![7]);
}

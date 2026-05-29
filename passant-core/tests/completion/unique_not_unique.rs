//! UNIQUE / NOT UNIQUE provenance semantics: scan vs grouped rewrites and execution.

#[path = "../common/duckdb.rs"]
mod duckdb;

use passant_core::{PolicyIr, Resolution, TableCatalog};

use crate::common::{assert_rewrite, pgn_policy, rewrite, rewrite_with_catalog};
use duckdb::TestDb;

fn not_unique_policy() -> PolicyIr {
    pgn_policy(&["Receipts"], "NOT UNIQUE Receipts.uid")
}

fn unique_email_policy() -> PolicyIr {
    PolicyIr::Pgn {
        sources: vec!["users".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "(COUNT(DISTINCT users.email) = 1)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }
}

#[test]
fn not_unique_scan_rewrite_fails_closed() {
    assert_rewrite(
        "SELECT uid FROM Receipts",
        &[not_unique_policy()],
        "SELECT uid FROM Receipts WHERE false",
    );
}

#[test]
fn not_unique_scan_execution_empty_for_any_row_count() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE Receipts (uid INTEGER)");
    db.exec("INSERT INTO Receipts VALUES (1)");
    db.register_policy(not_unique_policy());
    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT uid FROM Receipts ORDER BY uid"),
        Vec::<i64>::new()
    );
    db.exec("INSERT INTO Receipts VALUES (2)");
    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT uid FROM Receipts ORDER BY uid"),
        Vec::<i64>::new()
    );
}

#[test]
fn not_unique_grouped_rewrite_uses_having() {
    let sql = rewrite(
        "SELECT batch, count(*) AS n FROM Receipts GROUP BY batch",
        &[not_unique_policy()],
    );
    assert!(
        sql.contains("HAVING") && sql.contains("COUNT(DISTINCT Receipts.uid) <> 1"),
        "expected grouped NOT UNIQUE in HAVING: {sql}"
    );
}

#[test]
fn not_unique_grouped_execution_keeps_batches_with_multiple_uids() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE Receipts (batch INTEGER, uid INTEGER)");
    db.exec("INSERT INTO Receipts VALUES (1, 10), (1, 20), (2, 30)");
    db.register_policy(not_unique_policy());
    assert_eq!(
        db.rewrite_and_fetch_i64(
            "SELECT batch, count(*) AS n FROM Receipts GROUP BY batch ORDER BY batch",
        ),
        vec![1]
    );
}

#[test]
fn not_unique_grouped_execution_drops_singleton_uid_batches() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE Receipts (batch INTEGER, uid INTEGER)");
    db.exec("INSERT INTO Receipts VALUES (1, 10), (2, 20), (2, 30)");
    db.register_policy(not_unique_policy());
    assert_eq!(
        db.rewrite_and_fetch_i64(
            "SELECT batch, count(*) AS n FROM Receipts GROUP BY batch ORDER BY batch",
        ),
        vec![2]
    );
}

#[test]
fn unique_pgn_scan_rewrite_uses_global_cardinality_subquery() {
    let sql = rewrite("SELECT id, email FROM users", &[unique_email_policy()]);
    assert!(
        sql.contains("COUNT(DISTINCT users.email) = 1"),
        "expected global cardinality predicate: {sql}"
    );
    assert!(
        !sql.to_ascii_uppercase().contains("HAVING"),
        "scan UNIQUE should not use HAVING: {sql}"
    );
}

#[test]
fn unique_pgn_scan_execution_empty_when_multiple_emails() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE users (id INTEGER, email VARCHAR, dept VARCHAR)");
    db.exec("INSERT INTO users VALUES (1, 'a@example.com', 'eng'), (2, 'b@example.com', 'eng')");
    db.register_policy(unique_email_policy());
    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT id FROM users ORDER BY id"),
        Vec::<i64>::new()
    );
}

#[test]
fn unique_pgn_scan_execution_keeps_rows_when_single_email_value() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE users (id INTEGER, email VARCHAR)");
    db.exec("INSERT INTO users VALUES (1, 'alice@example.com'), (2, 'alice@example.com')");
    db.register_policy(unique_email_policy());
    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT id FROM users ORDER BY id"),
        vec![1, 2]
    );
}

#[test]
fn unique_pgn_grouped_rewrite_uses_having() {
    let sql = rewrite(
        "SELECT dept, count(*) AS n FROM users GROUP BY dept",
        &[unique_email_policy()],
    );
    assert!(
        sql.contains("HAVING") && sql.contains("COUNT(DISTINCT users.email) = 1"),
        "expected grouped UNIQUE in HAVING: {sql}"
    );
}

#[test]
fn unique_pgn_grouped_execution_keeps_departments_with_one_email() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE users (id INTEGER, email VARCHAR, dept VARCHAR)");
    db.exec(
        "INSERT INTO users VALUES (1, 'a@example.com', 'eng'), \
         (2, 'b@example.com', 'eng'), (3, 'solo@example.com', 'sales')",
    );
    db.register_policy(unique_email_policy());
    assert_eq!(
        db.rewrite_and_fetch_strings("SELECT dept FROM users GROUP BY dept ORDER BY dept",),
        vec!["sales".to_string()]
    );
}

#[test]
fn unique_equality_grouped_with_catalog_unique_adds_having_guard() {
    let mut catalog = TableCatalog::new();
    catalog.register_unique_column("users", "email");
    let policy = PolicyIr::Pgn {
        sources: vec!["users".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "users.email = 'alice@example.com'".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    let sql = rewrite_with_catalog(
        "SELECT dept FROM users GROUP BY dept ORDER BY dept",
        &[policy],
        catalog,
    );
    assert!(
        sql.contains("HAVING")
            && sql.contains("count(DISTINCT users.email) = 1")
            && sql.contains("min(users.email)"),
        "implicit uniqueness guard expected in grouped HAVING: {sql}"
    );
}

#[test]
fn count_distinct_eq_one_scan_global_cardinality_empty_when_many_ids() {
    let policy = pgn_policy(&["foo"], "count(distinct foo.id) = 1");
    let sql = rewrite("SELECT id FROM foo ORDER BY id", &[policy.clone()]);
    assert!(
        sql.contains("count(DISTINCT foo.id) = 1"),
        "expected global cardinality predicate: {sql}"
    );
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER)");
    db.exec("INSERT INTO foo VALUES (1), (NULL), (2)");
    db.register_policy(policy);
    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT id FROM foo ORDER BY id"),
        Vec::<i64>::new()
    );
}

#[test]
fn count_distinct_eq_one_scan_passes_when_table_has_one_distinct_id() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE foo (id INTEGER)");
    db.exec("INSERT INTO foo VALUES (1), (1)");
    db.register_policy(pgn_policy(&["foo"], "count(distinct foo.id) = 1"));
    assert_eq!(
        db.rewrite_and_fetch_i64("SELECT id FROM foo ORDER BY id"),
        vec![1, 1]
    );
}

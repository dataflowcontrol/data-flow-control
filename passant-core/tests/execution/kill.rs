use crate::common::pgn_policy_with;
use crate::duckdb::TestDb;
use passant_core::catalog::TableCatalog;
use passant_core::policy::parse_policy_text;
use passant_core::{PassantRewriter, PolicyIr, Resolution};

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
    db.exec("CREATE TABLE foo (id INTEGER)");
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

#[test]
fn min_meal_deduction_constant_biz_use_passes_for_ten_rows() {
    let mut catalog = TableCatalog::new();
    catalog.register_table(
        "Receipts",
        vec![
            "batch_id".into(),
            "id".into(),
            "cat".into(),
            "merchant".into(),
            "cost".into(),
        ],
    );
    catalog.register_table(
        "Expenses",
        vec![
            "id".into(),
            "merchant".into(),
            "cost".into(),
            "biz_use".into(),
            "source_note".into(),
        ],
    );
    let mut db = TestDb::new();
    db.rewriter = PassantRewriter::with_catalog(catalog);
    db.rewriter
        .register_validated_policy(
            parse_policy_text(
                "SOURCE Receipts SINK Expenses CONSTRAINT MIN(Receipts.cat) != 'Meal' OR Expenses.biz_use <= 50.0 ON FAIL KILL",
            )
            .expect("parse policy"),
        )
        .expect("register policy");

    db.exec(
        "CREATE TABLE Receipts (batch_id INTEGER, id INTEGER, cat VARCHAR, merchant VARCHAR, cost DOUBLE)",
    );
    db.exec(
        "CREATE TABLE Expenses (id INTEGER, merchant VARCHAR, cost DOUBLE, biz_use DOUBLE, source_note VARCHAR)",
    );
    for receipt_id in [2, 12, 22, 32, 42, 52, 62, 72, 82, 92] {
        db.exec(&format!(
            "INSERT INTO Receipts VALUES (0, {receipt_id}, 'Office', 'Blue Meridian 00-02', 100.0)"
        ));
    }

    let sql = "INSERT INTO Expenses (id, merchant, cost, biz_use, source_note)
WITH selected_receipts AS (
    SELECT * FROM (VALUES (1, 2), (2, 12), (3, 22), (4, 32), (5, 42), (6, 52), (7, 62), (8, 72), (9, 82), (10, 92)) AS selected_receipts(selection_position, receipt_id)
),
candidate_receipts AS (
    SELECT Receipts.batch_id AS report_id, Receipts.id AS receipt_id, Receipts.cat,
           Receipts.merchant, Receipts.cost, selected_receipts.selection_position
    FROM Receipts JOIN selected_receipts ON selected_receipts.receipt_id = Receipts.id
),
rollups AS (
    SELECT report_id, merchant, MIN(selection_position) AS output_position, SUM(cost) AS cost
    FROM candidate_receipts GROUP BY report_id, merchant
)
SELECT report_id * 1000 + output_position AS id, merchant, cost,
       100.0 AS biz_use,
       '10_row_queries:q00' AS source_note
FROM rollups ORDER BY output_position";

    db.rewrite_exec(sql);
    assert_eq!(
        db.fetchall_strings(
            "SELECT source_note FROM Expenses WHERE source_note = '10_row_queries:q00'"
        ),
        vec!["10_row_queries:q00".to_string()]
    );
}

#[test]
fn min_meal_deduction_constant_biz_use_kills_on_meal_rows() {
    let mut catalog = TableCatalog::new();
    catalog.register_table(
        "Receipts",
        vec![
            "batch_id".into(),
            "id".into(),
            "cat".into(),
            "merchant".into(),
            "cost".into(),
        ],
    );
    catalog.register_table(
        "Expenses",
        vec![
            "id".into(),
            "merchant".into(),
            "cost".into(),
            "biz_use".into(),
            "source_note".into(),
        ],
    );
    let mut db = TestDb::new();
    db.rewriter = PassantRewriter::with_catalog(catalog);
    db.rewriter
        .register_validated_policy(
            parse_policy_text(
                "SOURCE Receipts SINK Expenses CONSTRAINT MIN(Receipts.cat) != 'Meal' OR Expenses.biz_use <= 50.0 ON FAIL KILL",
            )
            .expect("parse policy"),
        )
        .expect("register policy");

    db.exec(
        "CREATE TABLE Receipts (batch_id INTEGER, id INTEGER, cat VARCHAR, merchant VARCHAR, cost DOUBLE)",
    );
    db.exec(
        "CREATE TABLE Expenses (id INTEGER, merchant VARCHAR, cost DOUBLE, biz_use DOUBLE, source_note VARCHAR)",
    );
    for receipt_id in [2, 12, 22, 32, 42, 52, 62, 72, 82, 92] {
        db.exec(&format!(
            "INSERT INTO Receipts VALUES (0, {receipt_id}, 'Meal', 'Cedar House 01-02', 100.0)"
        ));
    }

    let sql = "INSERT INTO Expenses (id, merchant, cost, biz_use, source_note)
WITH selected_receipts AS (
    SELECT * FROM (VALUES (1, 2), (2, 12), (3, 22), (4, 32), (5, 42), (6, 52), (7, 62), (8, 72), (9, 82), (10, 92)) AS selected_receipts(selection_position, receipt_id)
),
candidate_receipts AS (
    SELECT Receipts.batch_id AS report_id, Receipts.id AS receipt_id, Receipts.cat,
           Receipts.merchant, Receipts.cost, selected_receipts.selection_position
    FROM Receipts JOIN selected_receipts ON selected_receipts.receipt_id = Receipts.id
),
rollups AS (
    SELECT report_id, merchant, MIN(selection_position) AS output_position, SUM(cost) AS cost
    FROM candidate_receipts GROUP BY report_id, merchant
)
SELECT report_id * 1000 + output_position AS id, merchant, cost,
       100.0 AS biz_use,
       '10_row_queries:q01' AS source_note
FROM rollups ORDER BY output_position";

    let message = db.run_rewritten_expect_error(sql);
    assert!(message.contains("KILLing due to dfc policy violation"));
}

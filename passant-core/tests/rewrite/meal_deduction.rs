use passant_core::PassantRewriter;
use passant_core::aggregate_registry::AggregateFunctionSnapshot;
use passant_core::catalog::{CatalogSnapshot, CatalogTableInfo};
use passant_core::policy::parse_policy_text;
use sqlparser::ast::Statement;
use std::collections::HashMap;

fn meal_rewriter() -> PassantRewriter {
    let mut tables = HashMap::new();
    tables.insert(
        "Receipts".to_string(),
        CatalogTableInfo {
            columns: vec![
                "batch_id".into(),
                "id".into(),
                "cat".into(),
                "merchant".into(),
                "cost".into(),
            ],
            ..CatalogTableInfo::default()
        },
    );
    tables.insert(
        "Expenses".to_string(),
        CatalogTableInfo {
            columns: vec![
                "id".into(),
                "merchant".into(),
                "cost".into(),
                "biz_use".into(),
                "source_note".into(),
            ],
            ..CatalogTableInfo::default()
        },
    );
    let mut rewriter = PassantRewriter::new();
    rewriter.apply_catalog_snapshot(CatalogSnapshot {
        dialect: Some("duckdb".to_string()),
        tables,
        aggregate_functions: vec![AggregateFunctionSnapshot {
            name: "arbitrary".to_string(),
            schema: None,
            aliases: vec![],
            classification: None,
            source: Some("introspected".to_string()),
        }],
        ..CatalogSnapshot::default()
    });
    rewriter
        .register_validated_policy(
            parse_policy_text(
                "SOURCE Receipts SINK Expenses CONSTRAINT COUNT(DISTINCT Receipts.cat) = 1 AND (ARBITRARY(Receipts.cat) != 'Meal' OR Expenses.biz_use <= 50.0) ON FAIL KILL",
            )
            .expect("parse policy"),
        )
        .expect("register policy");
    rewriter
}

#[test]
fn meal_deduction_insert_rewrite_applies_output_scope_policy() {
    let rewriter = meal_rewriter();
    let sql = "INSERT INTO Expenses (id, merchant, cost, biz_use, source_note)
WITH selected_receipts AS (
    SELECT * FROM (VALUES (1, 102)) AS selected_receipts(selection_position, receipt_id)
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
       CASE WHEN merchant IN ('Cedar House 01-02') THEN 100.0 ELSE 50.0 END AS biz_use,
       '10_row_queries:q01' AS source_note
FROM rollups ORDER BY output_position";
    let rewritten = rewriter.rewrite(sql).expect("rewrite");
    assert!(
        !rewritten.contains("NOT Receipts.cat IS NULL"),
        "policy should not scan-transform COUNT DISTINCT at receipt scan: {rewritten}"
    );
    assert!(
        rewritten.contains("__passant_count_cat"),
        "expected aggregate propagation at rollups: {rewritten}"
    );
    assert!(
        rewritten.contains("passant_kill"),
        "expected kill wrap at outer select: {rewritten}"
    );
}

#[test]
fn meal_deduction_output_policy_rewrites_select_body_only() {
    let mut rewriter = meal_rewriter();
    rewriter
        .register_validated_policy(
            parse_policy_text(
                "SOURCE Receipts CONSTRAINT COUNT(DISTINCT Receipts.cat) = 1 AND (ARBITRARY(Receipts.cat) != 'Meal' OR _OUTPUT_.biz_use <= 50.0) ON FAIL KILL",
            )
            .expect("parse output policy"),
        )
        .expect("register output policy");
    let sql = "INSERT INTO Expenses (id, merchant, cost, biz_use, source_note)
WITH selected_receipts AS (
    SELECT * FROM (VALUES (1, 102)) AS selected_receipts(selection_position, receipt_id)
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
       CASE WHEN merchant IN ('Cedar House 01-02') THEN 100.0 ELSE 50.0 END AS biz_use,
       '10_row_queries:q01' AS source_note
FROM rollups ORDER BY output_position";
    let rewritten = rewriter.rewrite(sql).expect("rewrite");
    let Statement::Query(_) =
        sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::GenericDialect {}, &rewritten)
            .expect("parse rewritten")
            .pop()
            .expect("statement")
    else {
        panic!("expected select body only, got: {rewritten}");
    };
    assert!(rewritten.contains("passant_kill"));
}

fn min_meal_rewriter() -> PassantRewriter {
    let mut tables = HashMap::new();
    tables.insert(
        "Receipts".to_string(),
        CatalogTableInfo {
            columns: vec![
                "batch_id".into(),
                "id".into(),
                "cat".into(),
                "merchant".into(),
                "cost".into(),
            ],
            ..CatalogTableInfo::default()
        },
    );
    tables.insert(
        "Expenses".to_string(),
        CatalogTableInfo {
            columns: vec![
                "id".into(),
                "merchant".into(),
                "cost".into(),
                "biz_use".into(),
                "source_note".into(),
            ],
            ..CatalogTableInfo::default()
        },
    );
    let mut rewriter = PassantRewriter::new();
    rewriter.apply_catalog_snapshot(CatalogSnapshot {
        dialect: Some("duckdb".to_string()),
        tables,
        ..CatalogSnapshot::default()
    });
    rewriter
        .register_validated_policy(
            parse_policy_text(
                "SOURCE Receipts SINK Expenses CONSTRAINT MIN(Receipts.cat) != 'Meal' OR Expenses.biz_use <= 50.0 ON FAIL KILL",
            )
            .expect("parse policy"),
        )
        .expect("register policy");
    rewriter
}

const Q00_RECEIPT_IDS: &str =
    "(1, 2), (2, 12), (3, 22), (4, 32), (5, 42), (6, 52), (7, 62), (8, 72), (9, 82), (10, 92)";

fn q00_constant_biz_use_insert() -> String {
    format!(
        "INSERT INTO Expenses (id, merchant, cost, biz_use, source_note)
WITH selected_receipts AS (
    SELECT * FROM (VALUES {Q00_RECEIPT_IDS}) AS selected_receipts(selection_position, receipt_id)
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
FROM rollups ORDER BY output_position"
    )
}

#[test]
fn min_meal_deduction_defers_policy_to_outer_select() {
    let rewriter = min_meal_rewriter();
    let rewritten = rewriter
        .rewrite(&q00_constant_biz_use_insert())
        .expect("rewrite");
    assert!(
        !rewritten.contains("candidate_receipts AS (SELECT * FROM (WITH t1"),
        "policy kill wrap should not land on candidate_receipts: {rewritten}"
    );
    assert!(
        rewritten.contains("__passant_min_cat"),
        "expected MIN aggregate at rollups: {rewritten}"
    );
    assert!(
        rewritten.contains("max(CASE"),
        "expected relation-level kill guard: {rewritten}"
    );
    assert!(
        !rewritten.contains("OR CASE WHEN NOT"),
        "expected new kill filter shape: {rewritten}"
    );
}

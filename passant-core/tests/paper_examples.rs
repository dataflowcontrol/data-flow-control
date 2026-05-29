#[path = "common/mod.rs"]
mod common;

#[path = "common/duckdb.rs"]
mod duckdb;

use common::{assert_rewrite, pgn_policy, pgn_policy_with};
use duckdb::TestDb;
use passant_core::{PassantRewriter, Resolution, parse_policy_text};

#[test]
fn tax_agent_grounding_policy_rewrites_insert() {
    let policy = parse_policy_text(
        "SOURCE REQUIRED Receipts SINK Expenses CONSTRAINT Receipts.id = Expenses.id ON FAIL KILL",
    )
    .expect("grounding policy should parse");

    assert_rewrite(
        "INSERT INTO Expenses (id, item) SELECT Receipts.id, Receipts.item FROM Receipts",
        &[policy],
        "INSERT INTO Expenses (id, item) SELECT Receipts.id, Receipts.item FROM Receipts WHERE (Receipts.id = Receipts.id) OR kill()",
    );
}

#[test]
fn tax_agent_law_abiding_policy_rewrites_insert() {
    let policy = parse_policy_text(
        "SOURCE Receipts SINK Expenses CONSTRAINT Expenses.biz_use <= 50 OR Receipts.cat != 'Meal' ON FAIL REMOVE",
    )
    .expect("law-abiding policy should parse");

    assert_rewrite(
        "INSERT INTO Expenses (id, biz_use, cat) SELECT Receipts.id, Receipts.biz_use, Receipts.cat FROM Receipts",
        &[policy],
        "INSERT INTO Expenses (id, biz_use, cat) SELECT Receipts.id, Receipts.biz_use, Receipts.cat FROM Receipts WHERE Receipts.biz_use <= 50 OR Receipts.cat <> 'Meal'",
    );
}

#[test]
fn tax_agent_privacy_policy_rewrites_scan() {
    let policy = pgn_policy(&["Receipts"], "count(distinct Receipts.uid) > 1");

    assert_rewrite(
        "SELECT uid FROM Receipts",
        &[policy],
        "SELECT uid FROM Receipts WHERE Receipts.uid > 1",
    );
}

#[test]
fn k_anonymity_template_dominance_collapses_to_max_k() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(pgn_policy(
        &["Receipts"],
        "count(distinct Receipts.uid) > 2",
    ));
    rewriter.register_policy(pgn_policy(
        &["Receipts"],
        "count(distinct Receipts.uid) > 5",
    ));
    rewriter.register_policy(pgn_policy(
        &["Receipts"],
        "count(distinct Receipts.uid) > 3",
    ));

    let sql = rewriter
        .rewrite("SELECT uid FROM Receipts")
        .expect("dominated policies should rewrite");
    assert_eq!(sql, "SELECT uid FROM Receipts WHERE Receipts.uid > 5");
}

#[test]
fn state_machine_update_policy_execution() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE t (id INTEGER, state VARCHAR)");
    db.exec("INSERT INTO t VALUES (1, 'A')");
    db.register_policy(parse_policy_text(
        "SOURCE t AS t1 SINK t AS t2 CONSTRAINT count(distinct t1.id) = 1 AND max(t1.id) = t2.id AND case when max(t1.state) = 'A' then t2.state = 'B' when max(t1.state) = 'B' then t2.state in ('A', 'C') when max(t1.state) = 'C' then false end ON FAIL REMOVE",
    ).expect("state machine policy should parse"));

    db.rewrite_exec("UPDATE t AS t2 SET state = 'B' FROM t WHERE t.id = t2.id AND t.id = 1");
    assert_eq!(
        db.fetchall_strings("SELECT state FROM t WHERE id = 1"),
        vec!["B".to_string()]
    );
}

#[test]
fn tax_agent_grounding_execution_aborts_hallucinated_insert() {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE Receipts (id INTEGER, item VARCHAR)");
    db.exec("CREATE TABLE Expenses (id INTEGER, item VARCHAR)");
    db.exec("INSERT INTO Receipts VALUES (1, 'coffee')");
    db.register_policy(pgn_policy_with(
        &["Receipts"],
        "Receipts.id = Expenses.id",
        Resolution::Kill,
    ));

    let message = db.run_rewritten_expect_error(
        "INSERT INTO Expenses (id, item) SELECT 99, 'phantom' FROM Receipts",
    );
    assert!(message.contains("KILLing due to dfc policy violation"));
}

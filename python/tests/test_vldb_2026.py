"""Phase 1: VLDB 2026 paper PGN end-to-end examples (execution + results)."""

from __future__ import annotations

import duckdb
import pytest

from passant import Policy, Resolution, dfc
from passant.adapters.kill import KILL_MESSAGE

from flock_support import (
    ensure_flock_connection,
    flock_available,
)
from paper_helpers import assert_kill, make_conn, register_pgn

FLOCK_LLM_FILTER_CONSTRAINT = (
    "llm_filter({'model_name': 'default'}, "
    "{'prompt': 'Does this product description mention explosives?', "
    "'context_columns': [{'data': products.description}]})"
)


def test_non_leakage_privacy_constraint():
    conn = make_conn()
    conn.execute("CREATE TABLE Receipts (uid INTEGER, amount INTEGER)")
    conn.execute("CREATE TABLE catalog_users (id INTEGER, name VARCHAR)")
    conn.execute("CREATE TABLE catalog_roles (userid INTEGER, is_superuser BOOLEAN)")
    conn.execute("INSERT INTO Receipts VALUES (1, 10), (2, 30)")
    conn.execute("INSERT INTO catalog_users VALUES (1, 'alice'), (2, 'bob')")
    conn.execute("INSERT INTO catalog_roles VALUES (1, FALSE), (2, TRUE)")
    conn.execute("CREATE TABLE session_user AS SELECT 1 AS current_user_value")
    register_pgn(
        conn,
        "SOURCE Receipts "
        "DIMENSION catalog_users U, catalog_roles R, session_user S "
        "CONSTRAINT NOT UNIQUE Receipts.uid OR "
        "(S.current_user_value = U.id AND U.id = R.userid AND R.is_superuser) "
        "ON FAIL REMOVE",
    )
    rows = conn.fetchall("SELECT DISTINCT uid FROM Receipts ORDER BY uid")
    assert rows == [(2,)]


def test_grounded_derivation_kill_on_fabricated_expense():
    conn = make_conn()
    conn.execute("CREATE TABLE Receipts (id INTEGER, item VARCHAR)")
    conn.execute("CREATE TABLE Expenses (id INTEGER, item VARCHAR)")
    conn.execute("INSERT INTO Receipts VALUES (1, 'coffee')")
    register_pgn(
        conn,
        "SOURCE REQUIRED Receipts SINK Expenses CONSTRAINT Receipts.id = Expenses.id ON FAIL KILL",
    )
    conn.execute("INSERT INTO Expenses (id, item) SELECT Receipts.id, Receipts.item FROM Receipts")
    assert conn.fetchall("SELECT id, item FROM Expenses ORDER BY id") == [(1, "coffee")]
    assert_kill(
        conn,
        "INSERT INTO Expenses (id, item) SELECT 99, 'phantom' FROM Receipts",
    )


def test_law_abiding_transformation_removes_violating_meals():
    conn = make_conn()
    conn.execute("CREATE TABLE Receipts (id INTEGER, biz_use INTEGER, cat VARCHAR)")
    conn.execute("CREATE TABLE Expenses (id INTEGER, biz_use INTEGER, cat VARCHAR)")
    conn.execute("INSERT INTO Receipts VALUES (1, 80, 'Meal'), (2, 30, 'Meal'), (3, 80, 'Travel')")
    register_pgn(
        conn,
        "SOURCE Receipts SINK Expenses "
        "CONSTRAINT Expenses.biz_use <= 50 OR Receipts.cat != 'Meal' ON FAIL REMOVE",
    )
    conn.execute(
        "INSERT INTO Expenses (id, biz_use, cat) "
        "SELECT Receipts.id, Receipts.biz_use, Receipts.cat FROM Receipts"
    )
    assert conn.fetchall("SELECT id, biz_use, cat FROM Expenses ORDER BY id") == [
        (2, 30, "Meal"),
        (3, 80, "Travel"),
    ]


def test_fcra_protected_column_sink_kill():
    conn = make_conn()
    conn.execute("CREATE TABLE activeqs (qid INTEGER)")
    conn.execute("CREATE TABLE qcols (qid INTEGER, table_name VARCHAR, column_name VARCHAR)")
    conn.execute("CREATE TABLE ProtectedCols (table_name VARCHAR, column_name VARCHAR)")
    conn.execute("CREATE TABLE CreditDecisions (qid INTEGER, decision VARCHAR)")
    conn.execute("INSERT INTO activeqs VALUES (1), (2)")
    conn.execute(
        "INSERT INTO qcols VALUES (1, 'public_data', 'score'), (2, 'protected_data', 'ssn')"
    )
    conn.execute("INSERT INTO ProtectedCols VALUES ('protected_data', 'ssn')")
    register_pgn(
        conn,
        "SINK CreditDecisions "
        "DIMENSION activeqs Q, qcols C, ProtectedCols P "
        "CONSTRAINT Q.qid = C.qid AND NOT "
        "(C.table_name = P.table_name AND C.column_name = P.column_name) "
        "ON FAIL KILL",
    )
    conn.execute(
        "INSERT INTO CreditDecisions (qid, decision) "
        "SELECT 1, 'approve' FROM activeqs AS Q "
        "JOIN qcols AS C ON Q.qid = C.qid WHERE Q.qid = 1"
    )
    assert conn.fetchall("SELECT qid, decision FROM CreditDecisions ORDER BY qid") == [
        (1, "approve")
    ]
    with pytest.raises(Exception, match=KILL_MESSAGE):
        conn.execute(
            "INSERT INTO CreditDecisions (qid, decision) "
            "SELECT 2, 'deny' FROM activeqs AS Q "
            "JOIN qcols AS C ON Q.qid = C.qid WHERE Q.qid = 2"
        )


def test_state_machine_update_valid_transition():
    conn = make_conn()
    conn.execute("CREATE TABLE t (id INTEGER, state VARCHAR)")
    conn.execute("INSERT INTO t VALUES (1, 'A')")
    register_pgn(
        conn,
        "SOURCE t AS t1 SINK t AS t2 "
        "CONSTRAINT count(distinct t1.id) = 1 AND max(t1.id) = t2.id AND "
        "case when max(t1.state) = 'A' then t2.state = 'B' "
        "when max(t1.state) = 'B' then t2.state in ('A', 'C') "
        "when max(t1.state) = 'C' then false end ON FAIL REMOVE",
    )
    conn.execute("UPDATE t AS t2 SET state = 'B' FROM t WHERE t.id = t2.id AND t.id = 1")
    assert conn.fetchall("SELECT state FROM t WHERE id = 1 ORDER BY id") == [("B",)]


def test_templated_k_anonymity_dominating_threshold():
    conn = make_conn()
    conn.execute("CREATE TABLE Receipts (uid INTEGER)")
    conn.execute("INSERT INTO Receipts VALUES (1), (2), (3), (4), (5), (6)")
    for k in (2, 5, 3):
        register_pgn(
            conn,
            f"SOURCE Receipts CONSTRAINT count(distinct Receipts.uid) > {k} ON FAIL REMOVE",
        )
    rows = conn.fetchall("SELECT uid FROM Receipts ORDER BY uid")
    assert rows == [(6,)]


def _abort_on_violation(any_violation: bool) -> bool:
    if any_violation:
        raise ValueError(KILL_MESSAGE)
    return True


def test_relation_udf_aborts_select_when_total_exceeds_threshold():
    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE expenses (amount DOUBLE)")
    conn.execute("INSERT INTO expenses VALUES (40), (70)")
    conn.register_relation_resolution_function("abort_on_violation", _abort_on_violation)
    conn.register_policy(
        Policy(
            sources=["expenses"],
            constraint="max(expenses.amount) <= 60",
            on_fail=Resolution.RELATION_UDF,
            udf_name="abort_on_violation",
        )
    )
    rewritten = conn.transform_query("SELECT amount FROM expenses ORDER BY amount")
    assert "__passant_relation_input" in rewritten
    assert "abort_on_violation" in rewritten
    with pytest.raises(Exception, match=KILL_MESSAGE):
        conn.fetchall("SELECT amount FROM expenses ORDER BY amount")


@pytest.mark.flock
@pytest.mark.skipif(not flock_available(), reason="Flock DuckDB extension not available")
def test_flock_llm_filter_policy_registers_and_rewrites():
    conn = dfc(duckdb.connect())
    ensure_flock_connection(conn)
    conn.execute("CREATE TABLE products (name VARCHAR, description VARCHAR)")
    conn.execute("INSERT INTO products VALUES ('widget', 'A harmless widget')")
    conn.register_policy(
        Policy(
            sources=["products"],
            constraint=FLOCK_LLM_FILTER_CONSTRAINT,
            on_fail=Resolution.REMOVE,
        )
    )
    rewritten = conn.transform_query("SELECT name, description FROM products ORDER BY name")
    assert "llm_filter" in rewritten
    assert "products.description" in rewritten


def test_update_kill_uses_passant_kill_tuple_filter():
    conn = make_conn()
    conn.execute("CREATE TABLE t (id INTEGER, amount INTEGER)")
    conn.execute("INSERT INTO t VALUES (1, 200)")
    register_pgn(
        conn,
        "SOURCE t SINK t CONSTRAINT t.amount < 100 ON FAIL KILL",
    )
    rewritten = conn.transform_query("UPDATE t SET amount = 200 WHERE id = 1")
    assert "passant_kill" in rewritten


def test_self_join_policy_linear_application():
    conn = make_conn()
    conn.execute("CREATE TABLE foo (id INTEGER)")
    conn.execute("INSERT INTO foo VALUES (1), (2), (3)")
    register_pgn(
        conn,
        "SOURCE foo CONSTRAINT max(foo.id) > 1 ON FAIL REMOVE",
    )
    rows = conn.fetchall("SELECT a.id FROM foo AS a JOIN foo AS b ON a.id = b.id ORDER BY a.id")
    assert rows == [(2,), (3,)]
    explanation = conn.explain("SELECT a.id FROM foo AS a JOIN foo AS b ON a.id = b.id")
    assert len(explanation["applicable_policies"]) == 1

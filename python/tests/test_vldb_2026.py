"""VLDB 2026 paper PGN end-to-end examples (execution + rewrite regression)."""

from __future__ import annotations

import duckdb
import pytest
import sqlglot

from data_flow_control import Policy, Resolution, dfc
from data_flow_control.adapters.kill import KILL_MESSAGE

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


def assert_vldb_rewrite(conn, query: str, expected_sql: str) -> None:
    transformed = conn.transform_query(query)
    assert _pretty_sql(transformed) == _pretty_sql(expected_sql)


def _pretty_sql(sql: str) -> str:
    return sqlglot.parse_one(sql, read="duckdb").sql(dialect="duckdb", pretty=True)


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
    query = "SELECT DISTINCT uid FROM Receipts ORDER BY uid"
    assert_vldb_rewrite(
        conn,
        query,
        """
SELECT DISTINCT
  uid
FROM Receipts
CROSS JOIN "session_user" AS s
JOIN catalog_users AS u
  ON U.id = S.current_user_value
JOIN catalog_roles AS r
  ON R.userid = U.id
WHERE
  (
    Receipts.uid <> 1
  )
  OR (
    S.current_user_value = U.id AND U.id = R.userid AND R.is_superuser
  )
ORDER BY
  uid
""",
    )
    rows = conn.fetchall(query)
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
    insert_valid = "INSERT INTO Expenses (id, item) SELECT Receipts.id, Receipts.item FROM Receipts"
    assert_vldb_rewrite(
        conn,
        insert_valid,
        """
INSERT INTO Expenses (
  id,
  item
)
SELECT
  *
FROM (
  WITH t1 AS (
    SELECT
      Receipts.id,
      Receipts.item,
      Receipts.id = Receipts.id AS __passant_policy_pass
    FROM Receipts
  )
  SELECT
    t1.id,
    t1.item
  FROM t1
  WHERE
    t1.__passant_policy_pass
    OR CASE WHEN NOT t1.__passant_policy_pass THEN PASSANT_KILL() ELSE TRUE END
) AS __passant_kill
""",
    )
    insert_phantom = "INSERT INTO Expenses (id, item) SELECT 99, 'phantom' FROM Receipts"
    assert_vldb_rewrite(
        conn,
        insert_phantom,
        """
INSERT INTO Expenses (
  id,
  item
)
SELECT
  *
FROM (
  WITH t1 AS (
    SELECT
      99 AS expr_99,
      'phantom' AS phantom,
      Receipts.id = 99 AS __passant_policy_pass
    FROM Receipts
  )
  SELECT
    t1.expr_99,
    t1.phantom
  FROM t1
  WHERE
    t1.__passant_policy_pass
    OR CASE WHEN NOT t1.__passant_policy_pass THEN PASSANT_KILL() ELSE TRUE END
) AS __passant_kill
""",
    )
    conn.execute(insert_valid)
    assert conn.fetchall("SELECT id, item FROM Expenses ORDER BY id") == [(1, "coffee")]
    assert_kill(conn, insert_phantom)


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
    insert_sql = (
        "INSERT INTO Expenses (id, biz_use, cat) "
        "SELECT Receipts.id, Receipts.biz_use, Receipts.cat FROM Receipts"
    )
    assert_vldb_rewrite(
        conn,
        insert_sql,
        """
INSERT INTO Expenses (
  id,
  biz_use,
  cat
)
SELECT
  Receipts.id,
  Receipts.biz_use,
  Receipts.cat
FROM Receipts
WHERE
  Receipts.biz_use <= 50 OR Receipts.cat <> 'Meal'
""",
    )
    conn.execute(insert_sql)
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
    insert_ok = (
        "INSERT INTO CreditDecisions (qid, decision) "
        "SELECT 1, 'approve' FROM activeqs AS Q "
        "JOIN qcols AS C ON Q.qid = C.qid WHERE Q.qid = 1"
    )
    assert_vldb_rewrite(
        conn,
        insert_ok,
        """
INSERT INTO CreditDecisions (
  qid,
  decision
)
SELECT
  *
FROM (
  WITH t1 AS (
    SELECT
      1 AS expr_1,
      'approve' AS approve,
      Q.qid = C.qid
      AND NOT (
        C.table_name = P.table_name AND C.column_name = P.column_name
      ) AS __passant_policy_pass
    FROM activeqs AS Q
    JOIN qcols AS C
      ON Q.qid = C.qid
    CROSS JOIN ProtectedCols AS p
    WHERE
      Q.qid = 1
  )
  SELECT
    t1.expr_1,
    t1.approve
  FROM t1
  WHERE
    t1.__passant_policy_pass
    OR CASE WHEN NOT t1.__passant_policy_pass THEN PASSANT_KILL() ELSE TRUE END
) AS __passant_kill
""",
    )
    insert_kill = (
        "INSERT INTO CreditDecisions (qid, decision) "
        "SELECT 2, 'deny' FROM activeqs AS Q "
        "JOIN qcols AS C ON Q.qid = C.qid WHERE Q.qid = 2"
    )
    assert_vldb_rewrite(
        conn,
        insert_kill,
        """
INSERT INTO CreditDecisions (
  qid,
  decision
)
SELECT
  *
FROM (
  WITH t1 AS (
    SELECT
      2 AS expr_2,
      'deny' AS deny,
      Q.qid = C.qid
      AND NOT (
        C.table_name = P.table_name AND C.column_name = P.column_name
      ) AS __passant_policy_pass
    FROM activeqs AS Q
    JOIN qcols AS C
      ON Q.qid = C.qid
    CROSS JOIN ProtectedCols AS p
    WHERE
      Q.qid = 2
  )
  SELECT
    t1.expr_2,
    t1.deny
  FROM t1
  WHERE
    t1.__passant_policy_pass
    OR CASE WHEN NOT t1.__passant_policy_pass THEN PASSANT_KILL() ELSE TRUE END
) AS __passant_kill
""",
    )
    conn.execute(insert_ok)
    assert conn.fetchall("SELECT qid, decision FROM CreditDecisions ORDER BY qid") == [
        (1, "approve")
    ]
    with pytest.raises(Exception, match=KILL_MESSAGE):
        conn.execute(insert_kill)


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
    update_sql = "UPDATE t AS t2 SET state = 'B' FROM t WHERE t.id = t2.id AND t.id = 1"
    assert_vldb_rewrite(
        conn,
        update_sql,
        """
UPDATE t AS t2 SET state = 'B'
FROM t
WHERE
  t.id = t2.id
  AND t.id = 1
  AND t2.id = 1
  AND t2.id = t2.id
  AND CASE
    WHEN t2.state = 'A'
    THEN 'B' = 'B'
    WHEN t2.state = 'B'
    THEN t2.state IN ('A', 'C')
    WHEN t2.state = 'C'
    THEN FALSE
  END
""",
    )
    conn.execute(update_sql)
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
    query = "SELECT uid FROM Receipts ORDER BY uid"
    assert_vldb_rewrite(
        conn,
        query,
        """
SELECT
  uid
FROM Receipts
WHERE
  Receipts.uid > 5
ORDER BY
  uid
""",
    )
    rows = conn.fetchall(query)
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
    query = "SELECT amount FROM expenses ORDER BY amount"
    assert_vldb_rewrite(
        conn,
        query,
        """
WITH __passant_relation_input AS (
  SELECT
    amount,
    NOT expenses.amount <= 60 AS __passant_relation_violation
  FROM expenses
), __passant_relation_agg AS (
  SELECT
    BOOL_OR(__passant_relation_input.__passant_relation_violation) AS __passant_batch_violation
  FROM __passant_relation_input
)
SELECT
  __passant_relation_input.amount
FROM __passant_relation_input
CROSS JOIN __passant_relation_agg
WHERE
  abort_on_violation(__passant_relation_agg.__passant_batch_violation)
""",
    )
    with pytest.raises(Exception, match=KILL_MESSAGE):
        conn.fetchall(query)


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
    query = "SELECT name, description FROM products ORDER BY name"
    assert_vldb_rewrite(
        conn,
        query,
        """
SELECT
  name,
  description
FROM products
WHERE
  llm_filter(
    {'model_name': 'default'},
    {'prompt': 'Does this product description mention explosives?', 'context_columns': [{'data': products.description}]}
  )
ORDER BY
  name
""",
    )


def test_update_kill_uses_passant_kill_tuple_filter():
    conn = make_conn()
    conn.execute("CREATE TABLE t (id INTEGER, amount INTEGER)")
    conn.execute("INSERT INTO t VALUES (1, 200)")
    register_pgn(
        conn,
        "SOURCE t SINK t CONSTRAINT t.amount < 100 ON FAIL KILL",
    )
    update_sql = "UPDATE t SET amount = 200 WHERE id = 1"
    assert_vldb_rewrite(
        conn,
        update_sql,
        """
UPDATE t SET amount = 200
WHERE
  id = 1 AND 200 < 100 OR CASE WHEN NOT 200 < 100 THEN PASSANT_KILL() ELSE TRUE END
""",
    )


def test_self_join_policy_linear_application():
    conn = make_conn()
    conn.execute("CREATE TABLE foo (id INTEGER)")
    conn.execute("INSERT INTO foo VALUES (1), (2), (3)")
    register_pgn(
        conn,
        "SOURCE foo CONSTRAINT max(foo.id) > 1 ON FAIL REMOVE",
    )
    query = "SELECT a.id FROM foo AS a JOIN foo AS b ON a.id = b.id ORDER BY a.id"
    assert_vldb_rewrite(
        conn,
        query,
        """
SELECT
  a.id
FROM foo AS a
JOIN foo AS b
  ON a.id = b.id AND b.id > 1
WHERE
  a.id > 1
ORDER BY
  a.id
""",
    )
    rows = conn.fetchall(query)
    assert rows == [(2,), (3,)]
    explanation = conn.explain(query)
    assert len(explanation["applicable_policies"]) == 1

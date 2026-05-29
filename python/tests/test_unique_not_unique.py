"""UNIQUE / NOT UNIQUE: parser expansion, rewrite shape, and provenance-level execution."""

from __future__ import annotations

import duckdb

from data_flow_control import Policy, Resolution, dfc
from data_flow_control.catalog import build_catalog_snapshot


def test_policy_from_pgn_expands_unique_constraint():
    policy = Policy.from_pgn("SOURCE users CONSTRAINT UNIQUE users.email ON FAIL REMOVE")
    assert policy.constraint == "(COUNT(DISTINCT users.email) = 1)"


def test_policy_from_pgn_expands_not_unique_constraint():
    policy = Policy.from_pgn("SOURCE Receipts CONSTRAINT NOT UNIQUE Receipts.uid ON FAIL REMOVE")
    assert policy.constraint == "(COUNT(DISTINCT Receipts.uid) != 1)"


def test_unique_scan_rewrite_with_catalog_unique_column():
    conn = duckdb.connect()
    rewriter = dfc(conn)
    rewriter.execute("CREATE TABLE users (id INTEGER, email VARCHAR)")
    rewriter.planner.sync_catalog(
        build_catalog_snapshot(
            dialect="duckdb",
            tables={"users": {"columns": ["id", "email"]}},
            unique_columns=[["users", "email"]],
        )
    )
    policy = Policy(
        sources=["users"],
        constraint="users.email = 'alice@example.com'",
        on_fail=Resolution.REMOVE,
    )
    rewriter.planner.register_policy(policy)
    sql = "SELECT id, email FROM users"
    rewritten = rewriter.transform_query(sql)
    assert rewritten == "SELECT id, email FROM users WHERE users.email = 'alice@example.com'"


def test_not_unique_scan_rewrite():
    conn = duckdb.connect()
    rewriter = dfc(conn)
    rewriter.execute("CREATE TABLE Receipts (uid INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["Receipts"],
            constraint="NOT UNIQUE Receipts.uid",
            on_fail=Resolution.REMOVE,
        )
    )
    rewritten = rewriter.transform_query("SELECT uid FROM Receipts")
    assert rewritten == "SELECT uid FROM Receipts WHERE false"


def test_not_unique_scan_execution_empty_for_one_or_many_rows():
    conn = duckdb.connect()
    rewriter = dfc(conn)
    rewriter.execute("CREATE TABLE Receipts (uid INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["Receipts"],
            constraint="NOT UNIQUE Receipts.uid",
            on_fail=Resolution.REMOVE,
        )
    )
    rewriter.execute("INSERT INTO Receipts VALUES (1)")
    assert rewriter.fetchall("SELECT uid FROM Receipts ORDER BY uid") == []
    rewriter.execute("INSERT INTO Receipts VALUES (2)")
    assert rewriter.fetchall("SELECT uid FROM Receipts ORDER BY uid") == []


def test_not_unique_grouped_rewrite_uses_having():
    conn = duckdb.connect()
    rewriter = dfc(conn)
    rewriter.execute("CREATE TABLE Receipts (batch INTEGER, uid INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["Receipts"],
            constraint="NOT UNIQUE Receipts.uid",
            on_fail=Resolution.REMOVE,
        )
    )
    rewritten = rewriter.transform_query("SELECT batch, count(*) AS n FROM Receipts GROUP BY batch")
    assert "HAVING" in rewritten
    assert "COUNT(DISTINCT Receipts.uid) <> 1" in rewritten


def test_not_unique_grouped_execution_keeps_multi_uid_batches():
    conn = duckdb.connect()
    rewriter = dfc(conn)
    rewriter.execute("CREATE TABLE Receipts (batch INTEGER, uid INTEGER)")
    rewriter.execute("INSERT INTO Receipts VALUES (1, 10), (1, 20), (2, 30)")
    rewriter.register_policy(
        Policy(
            sources=["Receipts"],
            constraint="NOT UNIQUE Receipts.uid",
            on_fail=Resolution.REMOVE,
        )
    )
    assert rewriter.fetchall(
        "SELECT batch, count(*) AS n FROM Receipts GROUP BY batch ORDER BY batch"
    ) == [(1, 2)]


def test_not_unique_grouped_execution_drops_singleton_uid_batches():
    conn = duckdb.connect()
    rewriter = dfc(conn)
    rewriter.execute("CREATE TABLE Receipts (batch INTEGER, uid INTEGER)")
    rewriter.execute("INSERT INTO Receipts VALUES (1, 10), (2, 20), (2, 30)")
    rewriter.register_policy(
        Policy(
            sources=["Receipts"],
            constraint="NOT UNIQUE Receipts.uid",
            on_fail=Resolution.REMOVE,
        )
    )
    assert rewriter.fetchall(
        "SELECT batch, count(*) AS n FROM Receipts GROUP BY batch ORDER BY batch"
    ) == [(2, 2)]


def test_unique_pgn_scan_rewrite_uses_global_cardinality_predicate():
    conn = duckdb.connect()
    rewriter = dfc(conn)
    rewriter.execute("CREATE TABLE users (id INTEGER, email VARCHAR)")
    rewriter.register_policy(
        Policy.from_pgn("SOURCE users CONSTRAINT UNIQUE users.email ON FAIL REMOVE")
    )
    rewritten = rewriter.transform_query("SELECT id, email FROM users")
    assert "COUNT(DISTINCT users.email) = 1" in rewritten
    assert "HAVING" not in rewritten.upper()


def test_unique_pgn_scan_execution_empty_when_multiple_emails():
    conn = duckdb.connect()
    rewriter = dfc(conn)
    rewriter.execute("CREATE TABLE users (id INTEGER, email VARCHAR)")
    rewriter.execute("INSERT INTO users VALUES (1, 'a@example.com'), (2, 'b@example.com')")
    rewriter.register_policy(
        Policy.from_pgn("SOURCE users CONSTRAINT UNIQUE users.email ON FAIL REMOVE")
    )
    assert rewriter.fetchall("SELECT id FROM users ORDER BY id") == []


def test_unique_pgn_scan_execution_keeps_rows_when_single_email_value():
    conn = duckdb.connect()
    rewriter = dfc(conn)
    rewriter.execute("CREATE TABLE users (id INTEGER, email VARCHAR)")
    rewriter.execute("INSERT INTO users VALUES (1, 'alice@example.com'), (2, 'alice@example.com')")
    rewriter.register_policy(
        Policy.from_pgn("SOURCE users CONSTRAINT UNIQUE users.email ON FAIL REMOVE")
    )
    assert rewriter.fetchall("SELECT id FROM users ORDER BY id") == [(1,), (2,)]


def test_unique_pgn_grouped_rewrite_uses_having():
    conn = duckdb.connect()
    rewriter = dfc(conn)
    rewriter.execute("CREATE TABLE users (id INTEGER, email VARCHAR, dept VARCHAR)")
    rewriter.register_policy(
        Policy.from_pgn("SOURCE users CONSTRAINT UNIQUE users.email ON FAIL REMOVE")
    )
    rewritten = rewriter.transform_query("SELECT dept, count(*) AS n FROM users GROUP BY dept")
    assert "HAVING" in rewritten
    assert "COUNT(DISTINCT users.email) = 1" in rewritten


def test_unique_pgn_grouped_execution_keeps_departments_with_one_email():
    conn = duckdb.connect()
    rewriter = dfc(conn)
    rewriter.execute("CREATE TABLE users (id INTEGER, email VARCHAR, dept VARCHAR)")
    rewriter.execute(
        "INSERT INTO users VALUES "
        "(1, 'a@example.com', 'eng'), "
        "(2, 'b@example.com', 'eng'), "
        "(3, 'solo@example.com', 'sales')"
    )
    rewriter.register_policy(
        Policy.from_pgn("SOURCE users CONSTRAINT UNIQUE users.email ON FAIL REMOVE")
    )
    assert rewriter.fetchall("SELECT dept FROM users GROUP BY dept ORDER BY dept") == [("sales",)]


def test_unique_equality_grouped_with_catalog_unique_column():
    """Grouped implicit-uniqueness guard is per-group (HAVING), not a bare scan filter."""
    conn = duckdb.connect()
    rewriter = dfc(conn)
    rewriter.execute("CREATE TABLE users (id INTEGER, email VARCHAR, dept VARCHAR)")
    rewriter.execute(
        "INSERT INTO users VALUES "
        "(1, 'alice@example.com', 'eng'), "
        "(2, 'bob@example.com', 'eng'), "
        "(3, 'alice@example.com', 'sales')"
    )
    rewriter.planner.sync_catalog(
        build_catalog_snapshot(
            dialect="duckdb",
            tables={"users": {"columns": ["id", "email", "dept"]}},
            unique_columns=[["users", "email"]],
        )
    )
    rewriter.register_policy(
        Policy(
            sources=["users"],
            constraint="users.email = 'alice@example.com'",
            on_fail=Resolution.REMOVE,
        )
    )
    rewritten = rewriter.transform_query("SELECT dept FROM users GROUP BY dept ORDER BY dept")
    assert "HAVING" in rewritten
    assert "count(DISTINCT users.email) = 1" in rewritten
    assert "min(users.email)" in rewritten


def test_count_distinct_eq_one_scan_global_cardinality_empty_when_many_ids():
    conn = duckdb.connect()
    rewriter = dfc(conn)
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (NULL), (2)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="count(distinct foo.id) = 1",
            on_fail=Resolution.REMOVE,
        )
    )
    rewritten = rewriter.transform_query("SELECT id FROM foo ORDER BY id")
    assert "count(DISTINCT foo.id) = 1" in rewritten
    assert rewriter.fetchall(rewritten) == []


def test_count_distinct_eq_one_scan_passes_when_table_has_one_distinct_id():
    conn = duckdb.connect()
    rewriter = dfc(conn)
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1), (NULL), (1)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="count(distinct foo.id) = 1",
            on_fail=Resolution.REMOVE,
        )
    )
    assert rewriter.fetchall("SELECT id FROM foo ORDER BY id") == [(1,), (1,), (None,)]

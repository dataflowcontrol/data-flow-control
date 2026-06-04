from __future__ import annotations

import duckdb

from data_flow_control import Policy, Resolution, dfc

from paper_helpers import make_conn, register_pgn


def test_delete_executes_with_registered_policy():
    conn = make_conn()
    conn.execute("CREATE TABLE foo (id INTEGER)")
    conn.execute("INSERT INTO foo VALUES (1), (2), (3)")
    register_pgn(
        conn,
        "SOURCE foo CONSTRAINT max(foo.id) > 1 ON FAIL REMOVE",
    )
    conn.execute("DELETE FROM foo WHERE id = 2")
    assert conn.raw_connection.execute("SELECT id FROM foo ORDER BY id").fetchall() == [
        (1,),
        (3,),
    ]


def test_transform_query_passthrough_delete():
    conn = make_conn()
    conn.execute("CREATE TABLE foo (id INTEGER)")
    conn.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )
    sql = "DELETE FROM foo WHERE id = 1"
    assert conn.transform_query(sql) == sql


def test_delete_unaffected_by_dfc_connection():
    raw = duckdb.connect()
    raw.execute("CREATE TABLE foo (id INTEGER)")
    raw.execute("INSERT INTO foo VALUES (1), (2)")
    conn = dfc(raw)
    register_pgn(
        conn,
        "SOURCE foo CONSTRAINT max(foo.id) > 0 ON FAIL REMOVE",
    )
    conn.execute("DELETE FROM foo WHERE id = 1")
    assert conn.fetchall("SELECT id FROM foo ORDER BY id") == [(2,)]

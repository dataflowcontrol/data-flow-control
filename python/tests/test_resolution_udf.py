"""Tuple-level UDF resolution (CTE t1–t4) and passant_kill routing."""

from __future__ import annotations

import duckdb
import pytest

from data_flow_control import Policy, Resolution, dfc
from data_flow_control.adapters.kill import KILL_MESSAGE


def _keep_positive(id_value: int | None) -> int | None:
    if id_value is None or id_value < 0:
        return None
    return id_value


def test_tuple_udf_resolution_repairs_failing_rows():
    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE foo (id INTEGER)")
    conn.execute("INSERT INTO foo VALUES (-1), (1), (2)")
    conn.register_resolution_function("keep_positive", _keep_positive, ["BIGINT"], "BIGINT")
    conn.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) >= 0",
            on_fail=Resolution.UDF,
            udf_name="keep_positive",
        )
    )

    rewritten = conn.transform_query("SELECT id FROM foo ORDER BY id")
    assert "t1 AS" in rewritten
    assert "t2 AS" in rewritten
    assert "t3 AS" in rewritten
    assert "t4 AS" in rewritten
    assert "UNION" in rewritten
    assert "keep_positive" in rewritten

    assert conn.fetchall("SELECT id FROM foo ORDER BY id") == [(1,), (2,)]


def test_kill_uses_passant_kill_tuple_path():
    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE foo (id INTEGER)")
    conn.execute("INSERT INTO foo VALUES (1)")
    conn.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 10",
            on_fail=Resolution.KILL,
        )
    )

    rewritten = conn.transform_query("SELECT id FROM foo")
    assert "passant_kill" in rewritten
    assert "t1 AS" in rewritten
    assert "kill()" not in rewritten.lower() or "passant_kill" in rewritten

    with pytest.raises(Exception, match=KILL_MESSAGE):
        conn.fetchall("SELECT id FROM foo")

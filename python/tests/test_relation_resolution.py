"""Relation-level UDF resolution on INSERT SELECT."""

from __future__ import annotations

import duckdb
import pytest

from data_flow_control import Policy, Resolution, dfc
from data_flow_control.adapters.kill import KILL_MESSAGE


def _abort_on_violation(any_violation: bool) -> bool:
    if any_violation:
        raise ValueError(KILL_MESSAGE)
    return True


def test_relation_udf_aborts_insert_when_total_exceeds_threshold():
    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE expenses (amount DOUBLE)")
    conn.execute("CREATE TABLE reports (amount DOUBLE)")
    conn.register_relation_resolution_function("abort_on_violation", _abort_on_violation)
    conn.register_policy(
        Policy(
            sources=["expenses"],
            sink="reports",
            constraint="max(expenses.amount) <= 60",
            on_fail=Resolution.RELATION_UDF,
            udf_name="abort_on_violation",
        )
    )

    rewritten = conn.transform_query("INSERT INTO reports SELECT amount FROM expenses")
    assert "__passant_relation_input" in rewritten
    assert "abort_on_violation" in rewritten
    assert "abort_on_violation" in rewritten
    assert "bool_or" in rewritten

    conn.execute("DELETE FROM expenses")
    conn.execute("DELETE FROM reports")
    conn.execute("INSERT INTO expenses VALUES (40), (50)")
    conn.execute("INSERT INTO reports SELECT amount FROM expenses")
    assert conn.fetchall("SELECT amount FROM reports ORDER BY amount") == [(40.0,), (50.0,)]

    conn.execute("DELETE FROM expenses")
    conn.execute("DELETE FROM reports")
    conn.execute("INSERT INTO expenses VALUES (40), (70)")
    with pytest.raises(Exception, match=KILL_MESSAGE):
        conn.execute("INSERT INTO reports SELECT amount FROM expenses")
    assert conn.fetchall("SELECT amount FROM reports") == []


def test_relation_udf_rejected_on_update():
    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE reports (amount DOUBLE)")
    conn.register_relation_resolution_function("abort_on_violation", _abort_on_violation)
    conn.register_policy(
        Policy(
            sources=[],
            sink="reports",
            constraint="sum(reports.amount) <= 100",
            on_fail=Resolution.RELATION_UDF,
            udf_name="abort_on_violation",
        )
    )

    with pytest.raises(Exception, match="relation UDF resolution on UPDATE"):
        conn.transform_query("UPDATE reports SET amount = 200")

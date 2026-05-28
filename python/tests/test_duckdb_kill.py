"""DuckDB KILL resolution execution tests."""

from __future__ import annotations

import duckdb
import pytest

from passant import Policy, Resolution, wrap


def test_kill_resolution_aborts_query():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.execute("INSERT INTO foo VALUES (1)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="max(foo.id) > 10",
            on_fail=Resolution.KILL,
        )
    )

    with pytest.raises(Exception, match="KILLing due to dfc policy violation"):
        rewriter.fetchall("SELECT id FROM foo")


def test_update_kill_policy_aborts_invalid_assignment():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO reports VALUES (1, 'draft')")
    rewriter.register_policy(
        Policy(
            sources=[],
            sink="reports",
            constraint="reports.status = 'approved'",
            on_fail=Resolution.KILL,
        )
    )

    with pytest.raises(Exception, match="KILLing due to dfc policy violation"):
        rewriter.execute("UPDATE reports SET status = 'draft'")
    assert rewriter.fetchall("SELECT * FROM reports") == [(1, "draft")]

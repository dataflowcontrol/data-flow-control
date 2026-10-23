"""DuckDB KILL resolution execution tests."""

from __future__ import annotations

import duckdb
import pytest

from data_flow_control import Policy, Resolution, dfc


def test_kill_resolution_aborts_query():
    rewriter = dfc(duckdb.connect())
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
    rewriter = dfc(duckdb.connect())
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


def test_kill_function_is_not_pushed_below_source_filter():
    rewriter = dfc(duckdb.connect())
    raw = rewriter.raw_connection
    raw.execute("CREATE TABLE source (value VARCHAR)")
    raw.execute("INSERT INTO source VALUES ('allowed'), ('denied')")
    raw.execute("CREATE TABLE sink (value VARCHAR)")

    raw.execute(
        """
        INSERT INTO sink
        SELECT *
        FROM (
            WITH staged AS (
                SELECT value, value = 'allowed' AS policy_pass
                FROM source
                WHERE value = 'allowed'
            )
            SELECT value
            FROM staged
            WHERE policy_pass
                OR CASE WHEN NOT policy_pass THEN passant_kill() ELSE true END
        ) AS resolved
        """
    )

    assert raw.execute("SELECT * FROM sink").fetchall() == [("allowed",)]


@pytest.mark.parametrize(
    "rows,should_kill",
    [
        ([], False),
        ([(2,)], False),
        ([(0,)], True),
        ([(2,), (0,)], True),
    ],
)
@pytest.mark.parametrize("backend", ["duckdb", "sqlite"])
def test_kill_guard_handles_passing_failing_and_empty_inputs(backend, rows, should_kill):
    import sqlite3

    raw = duckdb.connect() if backend == "duckdb" else sqlite3.connect(":memory:")
    db = dfc(raw, dialect=backend)
    db.execute("CREATE TABLE source (x INTEGER)")
    if rows:
        raw.executemany("INSERT INTO source VALUES (?)", rows)
    db.register_policy(
        Policy(
            sources=["source"],
            constraint="MAX(source.x) > 1",
            on_fail=Resolution.KILL,
        )
    )
    if should_kill:
        message = "KILLing" if backend == "duckdb" else "user-defined function raised exception"
        with pytest.raises(Exception, match=message):
            db.fetchall("SELECT x FROM source")
    else:
        assert db.fetchall("SELECT x FROM source") == rows


@pytest.mark.parametrize("name", ["kill", "passant_kill"])
def test_existing_kill_udf_is_rejected(name):
    raw = duckdb.connect()
    raw.create_function(name, lambda: True, [], "BOOLEAN")
    with pytest.raises(RuntimeError, match="Cannot register DFC KILL function"):
        dfc(raw)
    # Preserve the user's function and roll back any partially registered DFC functions.
    assert raw.execute(f"SELECT {name}()").fetchall() == [(True,)]
    raw.remove_function(name)
    wrapped = dfc(raw)
    with pytest.raises(Exception, match="KILLing"):
        wrapped.raw_connection.execute("SELECT passant_kill()")


def test_rewrapping_the_same_connection_keeps_trusted_kill_udfs():
    raw = duckdb.connect()
    dfc(raw)
    wrapped = dfc(raw)
    with pytest.raises(Exception, match="KILLing"):
        wrapped.raw_connection.execute("SELECT passant_kill()")


def test_kill_registration_errors_are_not_silently_ignored():
    from data_flow_control.adapters.duckdb import DuckDBAdapter

    class FailingConnection:
        def create_function(self, *args, **kwargs):
            raise duckdb.Error("registration unavailable")

    with pytest.raises(RuntimeError, match="Cannot register DFC KILL function") as exc:
        DuckDBAdapter(FailingConnection()).register_kill_function()
    assert str(exc.value.__cause__) == "registration unavailable"


def test_kill_arithmetic_projections_have_valid_distinct_aliases():
    db = dfc(duckdb.connect())
    db.execute("CREATE TABLE items(x INTEGER)")
    db.execute("INSERT INTO items VALUES (2)")
    db.register_policy(
        Policy(
            sources=["items"],
            constraint="MAX(items.x) > 0",
            on_fail=Resolution.KILL,
        )
    )
    assert db.fetchall("SELECT x + 1, x - 1, 99 AS x_1 FROM items") == [(3, 1, 99)]

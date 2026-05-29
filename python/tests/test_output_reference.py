from __future__ import annotations

import duckdb
import pytest

from passant import Policy, Resolution, PassantRewriteError, dfc


def test_select_output_marker_policy_maps_projected_columns():
    rewriter = dfc(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="_OUTPUT_.status = 'approved' AND max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    assert rewriter.fetchall("SELECT id, status FROM foo ORDER BY id") == [(2, "approved")]


def test_insert_output_marker_policy_maps_inserted_values():
    rewriter = dfc(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER, status VARCHAR)")
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'draft'), (2, 'approved')")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            sink="reports",
            constraint="_OUTPUT_.status = 'approved' AND max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
    assert rewriter.fetchall("SELECT * FROM reports ORDER BY id") == [(2, "approved")]


def test_update_output_marker_maps_assigned_and_unassigned_columns():
    rewriter = dfc(duckdb.connect())
    rewriter.execute("CREATE TABLE reports (id INTEGER, status VARCHAR)")
    rewriter.execute("INSERT INTO reports VALUES (1, 'draft'), (2, 'draft')")
    rewriter.register_policy(
        Policy(
            sources=[],
            sink="reports",
            constraint="_OUTPUT_.status = 'approved' AND _OUTPUT_.id > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    rewriter.execute("UPDATE reports SET status = 'approved' WHERE id = 2")
    assert rewriter.fetchall("SELECT id, status FROM reports ORDER BY id") == [
        (1, "draft"),
        (2, "approved"),
    ]


def test_ambiguous_output_marker_reference_is_rejected():
    rewriter = dfc(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="_OUTPUT_.id > 1 AND max(foo.id) > 0",
            on_fail=Resolution.REMOVE,
        )
    )

    with pytest.raises(PassantRewriteError, match="ambiguous _OUTPUT_ column 'id'"):
        rewriter.transform_query("SELECT a.id, b.id FROM foo AS a JOIN foo AS b ON a.id = b.id")


def test_missing_output_marker_column_is_rejected():
    rewriter = dfc(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="_OUTPUT_.missing > 1",
            on_fail=Resolution.REMOVE,
        )
    )

    with pytest.raises(PassantRewriteError, match="not a projected output column"):
        rewriter.transform_query("SELECT foo.id FROM foo")

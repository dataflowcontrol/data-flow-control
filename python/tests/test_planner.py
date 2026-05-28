"""Planner unit tests without a live database connection."""

from __future__ import annotations

import duckdb
import pytest

from passant import Policy, Planner, Resolution, wrap
from passant.catalog import build_catalog_snapshot


def test_planner_rewrite_without_policies_passthrough():
    planner = Planner(dialect="duckdb")
    sql = "SELECT 1"
    assert planner.rewrite(sql) == sql


def test_planner_sync_catalog_accepts_normalized_snapshot():
    planner = Planner(dialect="sqlite")
    snapshot = build_catalog_snapshot(
        dialect="sqlite",
        tables={"foo": {"columns": ["id"], "types": {"id": "INTEGER"}}},
    )
    planner.sync_catalog(snapshot)
    planner.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.REMOVE)
    )
    assert planner.rewrite("SELECT id FROM foo") == "SELECT id FROM foo WHERE foo.id > 1"


def test_planner_rewrite_options_dialect_override():
    planner = Planner(dialect="duckdb")
    planner.sync_catalog(
        build_catalog_snapshot(
            dialect="duckdb",
            tables={"foo": {"columns": ["id"], "types": {"id": "INTEGER"}}},
        )
    )
    planner.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.REMOVE)
    )
    from passant.options import RewriteOptions

    rewritten = planner.rewrite(
        "SELECT id FROM foo",
        options=RewriteOptions(dialect="sqlite"),
    )
    assert rewritten == "SELECT id FROM foo WHERE foo.id > 1"


def test_planner_explain_dict_shape():
    db = wrap(duckdb.connect(), dialect="duckdb")
    db.execute("CREATE TABLE foo (id INTEGER)")
    db.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.REMOVE)
    )
    explanation = db.planner.explain_dict("SELECT id FROM foo")
    assert explanation["chosen"]["rewritten_sql"] == "SELECT id FROM foo WHERE foo.id > 1"
    db.close()


def test_clickhouse_kill_registration_fails():
    import pytest

    pytest.importorskip("clickhouse_connect")
    from passant.adapters.clickhouse import ClickHouseAdapter

    class _Client:
        database = "default"

        def query(self, *_args, **_kwargs):
            return type("R", (), {"result_rows": []})()

        def close(self):
            pass

    db = wrap(ClickHouseAdapter(_Client()), dialect="clickhouse")
    with pytest.raises(ValueError, match="exception_udf"):
        db.register_policy(
            Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.KILL)
        )


def test_umbra_kill_registration_fails():
    from passant.adapters.umbra import UmbraAdapter

    db = wrap(UmbraAdapter(object()), dialect="umbra")
    with pytest.raises(ValueError, match="exception_udf"):
        db.register_policy(
            Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.KILL)
        )

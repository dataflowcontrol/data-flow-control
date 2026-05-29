"""Planner unit tests without a live database connection."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

import duckdb
import pytest

from data_flow_control import Policy, Resolution, dfc
from data_flow_control.planner import Planner
from data_flow_control.adapters.base import Capabilities
from data_flow_control.catalog import build_catalog_snapshot
from data_flow_control.connection import Connection


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
    from data_flow_control.options import RewriteOptions

    rewritten = planner.rewrite(
        "SELECT id FROM foo",
        options=RewriteOptions(dialect="sqlite"),
    )
    assert rewritten == "SELECT id FROM foo WHERE foo.id > 1"


def test_planner_explain_dict_shape():
    db = dfc(duckdb.connect(), dialect="duckdb")
    db.execute("CREATE TABLE foo (id INTEGER)")
    db.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.REMOVE)
    )
    explanation = db.planner.explain_dict("SELECT id FROM foo")
    assert explanation["chosen"]["rewritten_sql"] == "SELECT id FROM foo WHERE foo.id > 1"
    db.close()


def test_umbra_kill_registration_fails():
    @dataclass
    class _UmbraAdapterLike:
        dialect: str = "umbra"
        capabilities: Capabilities = Capabilities(exception_udf=False)

        def execute(self, sql: str, params=None):
            raise AssertionError("execute should not be called")

        def introspect_catalog(self) -> dict:
            return build_catalog_snapshot(
                dialect=self.dialect,
                tables={"foo": {"columns": ["id"], "types": {"id": "INTEGER"}}},
            )

        def quote_identifier(self, name: str) -> str:
            return name

        def register_kill_function(self) -> None:
            raise AssertionError("register_kill_function should not be called")

        def register_resolution_function(
            self,
            name: str,
            func: Any,
            parameter_types: list[Any],
            return_type: Any,
        ) -> None:
            raise AssertionError("register_resolution_function should not be called")

        def register_relation_resolution_function(self, name: str, func: Any) -> None:
            raise AssertionError("register_relation_resolution_function should not be called")

        def close(self) -> None:
            return

    db = Connection(_UmbraAdapterLike(), planner=Planner(dialect="umbra"))
    with pytest.raises(ValueError, match="exception_udf"):
        db.register_policy(
            Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.KILL)
        )

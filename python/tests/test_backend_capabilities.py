"""Capability-gated registration and adapter edge cases."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

import duckdb
import pytest

from data_flow_control import Policy, Resolution, dfc
from data_flow_control.adapters.base import Capabilities
from data_flow_control.catalog import build_catalog_snapshot
from data_flow_control.connection import Connection
from data_flow_control.planner import Planner


@dataclass
class _DisabledKillAdapter:
    dialect: str = "test"
    capabilities: Capabilities = Capabilities(exception_udf=False)

    def execute(self, sql: str, params: Any = None):
        return duckdb.connect().execute(sql, params)

    def introspect_aggregate_functions(self) -> list[dict]:
        return []

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


def test_connection_rejects_kill_when_adapter_lacks_exception_udf():
    db = Connection(_DisabledKillAdapter(), planner=Planner(dialect="test"))
    with pytest.raises(ValueError, match="exception_udf"):
        db.register_policy(
            Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.KILL)
        )


@pytest.mark.umbra
def test_dfc_umbra_connection(passant_docker):
    psycopg = pytest.importorskip("psycopg")
    db = dfc(psycopg.connect(passant_docker.umbra_url), dialect="umbra")
    db.execute("CREATE TABLE passant_umbra_smoke (id INTEGER)")
    db.adapter.execute("DROP TABLE IF EXISTS passant_umbra_smoke")
    db.close()

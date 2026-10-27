"""Capability-gated registration and adapter edge cases."""

from __future__ import annotations

from dataclasses import dataclass
import re
from typing import Any
from unittest.mock import Mock

import duckdb
import pytest

from data_flow_control import Policy, Resolution, dfc
from data_flow_control.adapters.base import Capabilities
from data_flow_control.catalog import build_catalog_snapshot
from data_flow_control.connection import Connection
from data_flow_control.planner import Planner


@pytest.mark.parametrize("dialect", ["postgres", "clickhouse"])
def test_backend_registers_kill_function_used_by_rewriter(monkeypatch, dialect):
    from data_flow_control.adapters import clickhouse, postgres

    backend = Mock()
    if dialect == "postgres":
        monkeypatch.setattr(postgres, "psycopg", Mock())
        backend.autocommit = False
        adapter = postgres.PostgresAdapter(backend)
        ddl_calls = backend.cursor.return_value.execute.call_args_list
    else:
        monkeypatch.setattr(clickhouse, "clickhouse_connect", Mock())
        adapter = clickhouse.ClickHouseAdapter(backend)
        ddl_calls = backend.command.call_args_list

    db = Connection(adapter)
    db.planner.sync_catalog(
        build_catalog_snapshot(
            dialect=dialect,
            tables={"foo": {"columns": ["id"], "types": {"id": "INTEGER"}}},
        )
    )
    db.planner.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.KILL)
    )
    rewritten = db.planner.rewrite("SELECT id FROM foo")
    registered = {
        re.search(r"CREATE OR REPLACE FUNCTION (\w+)", call.args[0]).group(1) for call in ddl_calls
    }
    # Check the contract between the real rewriter and each adapter, including
    # the legacy name available to callers executing SQL directly.
    assert "passant_kill()" in rewritten
    assert {"kill", "passant_kill"} <= registered
    if dialect == "postgres":
        # KILL and RETRY each install their helper using the existing DDL contract.
        assert backend.commit.call_count == 2


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

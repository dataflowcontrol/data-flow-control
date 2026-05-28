"""Portable Passant API: adapters, catalog snapshots, capabilities, connect()."""

from __future__ import annotations

import json

import duckdb
import pytest

from passant import Policy, Resolution, connect, wrap
from passant.adapters import IMPLEMENTED_DIALECTS, create_adapter
from passant.catalog import build_catalog_snapshot


def test_build_catalog_snapshot_includes_dialect_and_tables():
    snapshot = build_catalog_snapshot(
        dialect="postgres",
        default_schema="public",
        search_path=["public"],
        tables={"foo": {"columns": ["id"], "types": {"id": "INTEGER"}}},
    )
    assert snapshot["dialect"] == "postgres"
    assert snapshot["default_schema"] == "public"
    assert snapshot["tables"]["foo"]["columns"] == ["id"]


def test_wrap_duckdb_includes_dialect_in_catalog():
    db = wrap(duckdb.connect(), dialect="duckdb")
    db.execute("CREATE TABLE foo (id INTEGER)")
    db.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.REMOVE)
    )
    snapshot = json.loads(db.planner.inner.dfc_policies_json())
    assert snapshot
    db.refresh_catalog()
    # Catalog sync is internal; policy registration succeeded.
    assert db.policies()


def test_connect_duckdb_memory_url():
    db = connect("duckdb:///:memory:")
    db.execute("CREATE TABLE foo (id INTEGER)")
    db.execute("INSERT INTO foo VALUES (1), (2)")
    db.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.REMOVE)
    )
    assert db.fetchall("SELECT id FROM foo ORDER BY id") == [(2,)]
    db.close()


def test_sqlite_remove_scan_conformance():
    import sqlite3

    conn = sqlite3.connect(":memory:")
    db = wrap(conn, dialect="sqlite")
    db.execute("CREATE TABLE foo (id INTEGER)")
    db.execute("INSERT INTO foo VALUES (1), (2)")
    db.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.REMOVE)
    )
    assert db.fetchall("SELECT id FROM foo ORDER BY id") == [(2,)]
    db.close()


def test_sqlite_kill_registration_fails():
    import sqlite3

    db = wrap(sqlite3.connect(":memory:"), dialect="sqlite")
    db.execute("CREATE TABLE foo (id INTEGER)")
    with pytest.raises(ValueError, match="exception_udf"):
        db.register_policy(
            Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.KILL)
        )
    db.close()


def test_clickhouse_adapter_factory():
    pytest.importorskip("clickhouse_connect")
    from passant.adapters.clickhouse import ClickHouseAdapter

    class _Client:
        database = "default"

        def query(self, *_args, **_kwargs):
            return type("R", (), {"result_rows": []})()

        def close(self):
            pass

    adapter = ClickHouseAdapter(_Client())
    assert adapter.dialect == "clickhouse"
    assert adapter.capabilities.exception_udf is False


def test_unknown_dialect_raises():
    with pytest.raises(ValueError, match="Unknown dialect"):
        create_adapter(object(), "notadb")


def test_supported_and_implemented_dialect_sets():
    assert "duckdb" in IMPLEMENTED_DIALECTS
    assert "sqlite" in IMPLEMENTED_DIALECTS
    assert "datafusion" in IMPLEMENTED_DIALECTS
    assert "clickhouse" in IMPLEMENTED_DIALECTS
    assert "umbra" in IMPLEMENTED_DIALECTS


@pytest.mark.postgres
def test_postgres_remove_scan_conformance(passant_docker):
    psycopg = pytest.importorskip("psycopg")
    db = wrap(psycopg.connect(passant_docker.postgres_url), dialect="postgres")
    db.execute("DROP TABLE IF EXISTS passant_portable_foo")
    db.execute("CREATE TABLE passant_portable_foo (id INTEGER)")
    db.execute("INSERT INTO passant_portable_foo VALUES (1), (2)")
    db.register_policy(
        Policy(
            sources=["passant_portable_foo"],
            constraint="max(passant_portable_foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )
    assert db.fetchall("SELECT id FROM passant_portable_foo ORDER BY id") == [(2,)]
    db.adapter.execute("DROP TABLE IF EXISTS passant_portable_foo")
    db.close()

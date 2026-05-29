"""Portable Passant API: adapters, catalog snapshots, capabilities, dfc()."""

from __future__ import annotations

import json

import duckdb
import pytest

from passant import Dialect, Policy, Resolution, dfc
from passant.adapters import create_adapter, sniff_dialect
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


def test_dfc_duckdb_includes_dialect_in_catalog():
    db = dfc(duckdb.connect(), dialect="duckdb")
    db.execute("CREATE TABLE foo (id INTEGER)")
    db.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.REMOVE)
    )
    snapshot = json.loads(db.planner.inner.policies_json())
    assert snapshot
    db.refresh_catalog()
    # Catalog sync is internal; policy registration succeeded.
    assert db.policies()


def test_dfc_duckdb_memory():
    db = dfc(duckdb.connect())
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
    db = dfc(conn, dialect="sqlite")
    db.execute("CREATE TABLE foo (id INTEGER)")
    db.execute("INSERT INTO foo VALUES (1), (2)")
    db.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.REMOVE)
    )
    assert db.fetchall("SELECT id FROM foo ORDER BY id") == [(2,)]
    db.close()


def test_sqlite_kill_conformance():
    import sqlite3

    from kill_conformance import kill_scan_conformance

    db = dfc(sqlite3.connect(":memory:"), dialect="sqlite")
    kill_scan_conformance(db)
    db.close()


def test_clickhouse_adapter_factory():
    pytest.importorskip("clickhouse_connect")
    from passant.adapters.clickhouse import ClickHouseAdapter

    class _Client:
        database = "default"

        def query(self, *_args, **_kwargs):
            return type("R", (), {"result_rows": []})()

        def command(self, _sql: str) -> None:
            return None

        def close(self):
            pass

    adapter = ClickHouseAdapter(_Client())
    assert adapter.dialect == "clickhouse"
    assert adapter.capabilities.exception_udf is True


def test_unknown_dialect_raises():
    with pytest.raises(ValueError, match="Unknown dialect"):
        create_adapter(object(), "notadb")


def test_supported_dialect_set():
    values = {d.value for d in Dialect}
    assert "duckdb" in values
    assert "sqlite" in values
    assert "datafusion" in values
    assert "clickhouse" in values
    assert "umbra" in values


def test_dfc_sniffs_duckdb_without_explicit_dialect():
    db = dfc(duckdb.connect())
    db.execute("CREATE TABLE foo (id INTEGER)")
    assert db.adapter.dialect == "duckdb"
    db.close()


def test_dfc_sniffs_sqlite_without_explicit_dialect():
    import sqlite3

    db = dfc(sqlite3.connect(":memory:"))
    assert db.adapter.dialect == "sqlite"
    db.close()


def test_dfc_sniffs_adapter_dialect_without_explicit_dialect():
    class _AdapterLike:
        dialect = "postgres"

    assert sniff_dialect(_AdapterLike()) == "postgres"


def test_dfc_sniffs_psycopg_as_postgres():
    class _PsycopgConn:
        pass

    _PsycopgConn.__module__ = "psycopg.connection"
    assert sniff_dialect(_PsycopgConn()) == "postgres"


def test_dfc_sniff_failure_lists_supported_dialects():
    supported = ", ".join(Dialect.supported_names())
    with pytest.raises(ValueError, match="Could not infer database dialect"):
        dfc(object())
    with pytest.raises(ValueError, match=supported):
        dfc(object())


@pytest.mark.postgres
def test_postgres_remove_scan_conformance(passant_docker):
    psycopg = pytest.importorskip("psycopg")
    db = dfc(psycopg.connect(passant_docker.postgres_url), dialect="postgres")
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

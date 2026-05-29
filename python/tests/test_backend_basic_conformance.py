"""Shared backend conformance tests for the portable Passant API."""

from __future__ import annotations

import sqlite3

import duckdb
import pytest

from passant import Policy, Resolution, dfc
from kill_conformance import kill_scan_conformance


def _remove_scan_conformance(db) -> None:
    db.adapter.execute("DROP TABLE IF EXISTS foo")
    db.execute("CREATE TABLE foo (id INTEGER)")
    db.execute("INSERT INTO foo VALUES (1), (2)")
    db.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.REMOVE)
    )
    assert db.fetchall("SELECT id FROM foo ORDER BY id") == [(2,)]


def test_duckdb_remove_scan_conformance():
    db = dfc(duckdb.connect(), dialect="duckdb")
    _remove_scan_conformance(db)
    db.close()


def test_duckdb_kill_conformance():
    db = dfc(duckdb.connect(), dialect="duckdb")
    kill_scan_conformance(db)
    db.close()


def test_sqlite_remove_scan_conformance():
    db = dfc(sqlite3.connect(":memory:"), dialect="sqlite")
    _remove_scan_conformance(db)
    db.close()


def test_sqlite_kill_conformance():
    db = dfc(sqlite3.connect(":memory:"), dialect="sqlite")
    kill_scan_conformance(db)
    db.close()


def test_dfc_sqlite_memory():
    db = dfc(sqlite3.connect(":memory:"))
    _remove_scan_conformance(db)
    db.close()


def test_datafusion_remove_scan_conformance():
    datafusion = pytest.importorskip("datafusion")
    import pyarrow as pa

    ctx = datafusion.SessionContext()
    ctx.register_record_batches(
        "foo", [pa.table({"id": pa.array([1, 2], type=pa.int64())}).to_batches()]
    )
    db = dfc(ctx, dialect="datafusion")
    db.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.REMOVE)
    )
    assert db.fetchall("SELECT id FROM foo ORDER BY id") == [(2,)]
    db.close()


def test_datafusion_kill_conformance():
    datafusion = pytest.importorskip("datafusion")
    import pyarrow as pa

    ctx = datafusion.SessionContext()
    ctx.register_record_batches(
        "foo", [pa.table({"id": pa.array([1], type=pa.int64())}).to_batches()]
    )
    db = dfc(ctx, dialect="datafusion")
    kill_scan_conformance(db, skip_ddl=True)
    db.close()


def test_datafusion_dfc():
    datafusion = pytest.importorskip("datafusion")
    db = dfc(datafusion.SessionContext())
    import pyarrow as pa

    db.raw_connection.register_record_batches(
        "foo", [pa.table({"id": pa.array([1, 2], type=pa.int64())}).to_batches()]
    )
    db.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.REMOVE)
    )
    assert db.fetchall("SELECT id FROM foo ORDER BY id") == [(2,)]
    db.close()


@pytest.mark.postgres
def test_postgres_remove_scan_conformance(passant_docker):
    psycopg = pytest.importorskip("psycopg")
    db = dfc(psycopg.connect(passant_docker.postgres_url), dialect="postgres")
    db.execute("DROP TABLE IF EXISTS passant_conformance_foo")
    db.execute("CREATE TABLE passant_conformance_foo (id INTEGER)")
    db.execute("INSERT INTO passant_conformance_foo VALUES (1), (2)")
    db.register_policy(
        Policy(
            sources=["passant_conformance_foo"],
            constraint="max(passant_conformance_foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )
    assert db.fetchall("SELECT id FROM passant_conformance_foo ORDER BY id") == [(2,)]
    db.adapter.execute("DROP TABLE IF EXISTS passant_conformance_foo")
    db.close()


@pytest.mark.postgres
def test_postgres_kill_conformance(passant_docker):
    psycopg = pytest.importorskip("psycopg")
    db = dfc(psycopg.connect(passant_docker.postgres_url), dialect="postgres")
    db.execute("DROP TABLE IF EXISTS passant_conformance_kill_foo")
    kill_scan_conformance(db, table="passant_conformance_kill_foo")
    db.adapter.execute("DROP TABLE IF EXISTS passant_conformance_kill_foo")
    db.close()


@pytest.mark.clickhouse
def test_clickhouse_remove_scan_conformance(passant_docker):
    clickhouse_connect = pytest.importorskip("clickhouse_connect")
    from docker_services import _parse_clickhouse_url

    client = clickhouse_connect.get_client(**_parse_clickhouse_url(passant_docker.clickhouse_url))
    db = dfc(client)
    db.execute("DROP TABLE IF EXISTS passant_conformance_foo")
    db.execute("CREATE TABLE passant_conformance_foo (id Int32) ENGINE=Memory")
    db.execute("INSERT INTO passant_conformance_foo VALUES (1), (2)")
    db.register_policy(
        Policy(
            sources=["passant_conformance_foo"],
            constraint="max(passant_conformance_foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )
    assert db.fetchall("SELECT id FROM passant_conformance_foo ORDER BY id") == [(2,)]
    db.adapter.execute("DROP TABLE IF EXISTS passant_conformance_foo")
    db.close()


@pytest.mark.clickhouse
def test_clickhouse_kill_conformance(passant_docker):
    clickhouse_connect = pytest.importorskip("clickhouse_connect")
    from docker_services import _parse_clickhouse_url

    client = clickhouse_connect.get_client(**_parse_clickhouse_url(passant_docker.clickhouse_url))
    db = dfc(client)
    db.execute("DROP TABLE IF EXISTS passant_conformance_kill_foo")
    kill_scan_conformance(db, table="passant_conformance_kill_foo")
    db.adapter.execute("DROP TABLE IF EXISTS passant_conformance_kill_foo")
    db.close()


@pytest.mark.umbra
def test_umbra_remove_scan_conformance(passant_docker):
    psycopg = pytest.importorskip("psycopg")
    db = dfc(psycopg.connect(passant_docker.umbra_url), dialect="umbra")
    _remove_scan_conformance(db)
    db.close()


@pytest.mark.umbra
def test_umbra_kill_conformance(passant_docker):
    pytest.skip("Umbra does not support CREATE OR REPLACE FUNCTION (PL/pgSQL) for kill()")

"""Shared backend conformance tests for the portable Passant API."""

from __future__ import annotations

import sqlite3

import duckdb
import pytest

from passant import Policy, Resolution, connect, wrap


def _remove_scan_conformance(db) -> None:
    db.adapter.execute("DROP TABLE IF EXISTS foo")
    db.execute("CREATE TABLE foo (id INTEGER)")
    db.execute("INSERT INTO foo VALUES (1), (2)")
    db.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.REMOVE)
    )
    assert db.fetchall("SELECT id FROM foo ORDER BY id") == [(2,)]


def test_duckdb_remove_scan_conformance():
    db = wrap(duckdb.connect(), dialect="duckdb")
    _remove_scan_conformance(db)
    db.close()


def test_duckdb_kill_conformance():
    db = wrap(duckdb.connect(), dialect="duckdb")
    db.execute("CREATE TABLE foo (id INTEGER)")
    db.execute("INSERT INTO foo VALUES (1)")
    db.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.KILL)
    )
    with pytest.raises(Exception, match="KILLing due to dfc policy violation"):
        db.fetchall("SELECT id FROM foo")
    db.close()


def test_sqlite_remove_scan_conformance():
    db = wrap(sqlite3.connect(":memory:"), dialect="sqlite")
    _remove_scan_conformance(db)
    db.close()


def test_sqlite_kill_registration_fails():
    db = wrap(sqlite3.connect(":memory:"), dialect="sqlite")
    db.execute("CREATE TABLE foo (id INTEGER)")
    with pytest.raises(ValueError, match="exception UDF"):
        db.register_policy(
            Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.KILL)
        )
    db.close()


def test_connect_sqlite_memory_url():
    db = connect("sqlite:///:memory:")
    _remove_scan_conformance(db)
    db.close()


def test_datafusion_remove_scan_conformance():
    datafusion = pytest.importorskip("datafusion")
    import pyarrow as pa

    ctx = datafusion.SessionContext()
    ctx.register_record_batches(
        "foo", [pa.table({"id": pa.array([1, 2], type=pa.int64())}).to_batches()]
    )
    db = wrap(ctx, dialect="datafusion")
    db.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.REMOVE)
    )
    assert db.fetchall("SELECT id FROM foo ORDER BY id") == [(2,)]
    db.close()


def test_datafusion_kill_registration_fails():
    datafusion = pytest.importorskip("datafusion")
    db = wrap(datafusion.SessionContext(), dialect="datafusion")
    with pytest.raises(ValueError, match="exception UDF"):
        db.register_policy(
            Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.KILL)
        )
    db.close()


def test_datafusion_connect():
    pytest.importorskip("datafusion")
    db = connect("datafusion://")
    import pyarrow as pa

    db.connection.register_record_batches(
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
    db = wrap(psycopg.connect(passant_docker.postgres_url), dialect="postgres")
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


@pytest.mark.clickhouse
def test_clickhouse_remove_scan_conformance(passant_docker):
    pytest.importorskip("clickhouse_connect")
    db = connect(passant_docker.clickhouse_url)
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


def test_postgres_kill_registration_fails():
    pytest.importorskip("psycopg")
    from passant.adapters.postgres import PostgresAdapter

    db = wrap(PostgresAdapter(object()), dialect="postgres")
    with pytest.raises(ValueError, match="exception UDF"):
        db.register_policy(
            Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.KILL)
        )


@pytest.mark.umbra
def test_umbra_remove_scan_conformance(passant_docker):
    db = connect(passant_docker.umbra_url, dialect="umbra")
    _remove_scan_conformance(db)
    db.close()


def test_umbra_kill_registration_fails():
    from passant.adapters.umbra import UmbraAdapter

    db = wrap(UmbraAdapter(object()), dialect="umbra")
    with pytest.raises(ValueError, match="exception UDF"):
        db.register_policy(
            Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.KILL)
        )

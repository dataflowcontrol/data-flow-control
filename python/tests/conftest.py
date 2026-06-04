"""Shared fixtures for Passant Python tests."""

from __future__ import annotations

import duckdb
import pytest

from data_flow_control import dfc
from flock_support import flock_available, load_flock


@pytest.fixture
def rewriter():
    """Create a Passant connection with standard test tables."""
    db = dfc(duckdb.connect())

    db.execute("CREATE TABLE foo (id INTEGER, name VARCHAR)")
    db.execute("INSERT INTO foo VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Charlie')")
    db.execute("ALTER TABLE foo ADD COLUMN bar VARCHAR")
    db.execute("UPDATE foo SET bar = 'value' || id::VARCHAR")

    db.execute("CREATE TABLE baz (x INTEGER, y VARCHAR)")
    db.execute("INSERT INTO baz VALUES (10, 'test')")

    yield db

    db.close()


@pytest.fixture(scope="session")
def flock_extension():
    """Install and load Flock once per test session when available."""
    if not flock_available():
        pytest.skip("Flock DuckDB extension not available (run passant/scripts/setup_flock.sh)")
    conn = duckdb.connect()
    load_flock(conn)
    yield conn
    conn.close()


@pytest.fixture(scope="session")
def passant_docker():
    """Start local Docker services used by postgres/clickhouse/umbra tests."""
    from tests.docker_services import PassantDockerStack

    try:
        stack = PassantDockerStack.start()
    except Exception as exc:
        pytest.skip(f"Passant docker services unavailable: {exc}")
    return stack

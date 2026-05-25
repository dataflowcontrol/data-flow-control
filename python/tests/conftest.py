"""Shared fixtures for Passant Python tests."""

import duckdb
import pytest

from passant import wrap


@pytest.fixture
def rewriter():
    """Create a Passant connection with standard test tables."""
    db = wrap(duckdb.connect())

    db.execute("CREATE TABLE foo (id INTEGER, name VARCHAR)")
    db.execute("INSERT INTO foo VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Charlie')")
    db.execute("ALTER TABLE foo ADD COLUMN bar VARCHAR")
    db.execute("UPDATE foo SET bar = 'value' || id::VARCHAR")

    db.execute("CREATE TABLE baz (x INTEGER, y VARCHAR)")
    db.execute("INSERT INTO baz VALUES (10, 'test')")

    yield db

    db.close()

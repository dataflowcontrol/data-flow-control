"""Shared fixtures for Passant Python tests."""

import pytest

from passant.compat import SQLRewriter


@pytest.fixture
def rewriter():
    """Create a Passant SQLRewriter with standard test tables."""
    rewriter = SQLRewriter()

    rewriter.execute("CREATE TABLE foo (id INTEGER, name VARCHAR)")
    rewriter.execute("INSERT INTO foo VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Charlie')")
    rewriter.execute("ALTER TABLE foo ADD COLUMN bar VARCHAR")
    rewriter.execute("UPDATE foo SET bar = 'value' || id::VARCHAR")

    rewriter.execute("CREATE TABLE baz (x INTEGER, y VARCHAR)")
    rewriter.execute("INSERT INTO baz VALUES (10, 'test')")

    yield rewriter

    rewriter.close()

"""Connection wrapper: raw_connection and explain."""

from __future__ import annotations

import duckdb

from passant import Policy, Resolution, wrap


def test_raw_connection_exposes_duckdb_connection():
    db = wrap(duckdb.connect())
    assert db.raw_connection is db.adapter.connection


def test_explain_returns_dict():
    db = wrap(duckdb.connect())
    db.execute("CREATE TABLE foo (id INTEGER)")
    db.register_policy(
        Policy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.REMOVE)
    )
    explanation = db.explain("SELECT id FROM foo")
    assert explanation["chosen"]["rewritten_sql"] == "SELECT id FROM foo WHERE foo.id > 1"

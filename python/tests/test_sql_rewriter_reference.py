"""Compare Passant rewrites against the original sql_rewriter reference implementation."""

from __future__ import annotations

import pytest
from sqlglot import parse_one

from passant.compat import DFCPolicy, Resolution, SQLRewriter as PassantSQLRewriter
from sql_rewriter import DFCPolicy as LegacyDFCPolicy
from sql_rewriter import Resolution as LegacyResolution
from sql_rewriter import SQLRewriter as LegacySQLRewriter


def _normalize(sql: str) -> str:
    return parse_one(sql, read="duckdb").sql(pretty=True, dialect="duckdb")


@pytest.fixture
def paired_rewriters():
    passant = PassantSQLRewriter()
    legacy = LegacySQLRewriter()
    for rewriter in (passant, legacy):
        rewriter.execute("CREATE TABLE foo (id INTEGER, name VARCHAR)")
        rewriter.execute("INSERT INTO foo VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Charlie')")
        rewriter.execute("CREATE TABLE baz (x INTEGER)")
        rewriter.execute("INSERT INTO baz VALUES (10), (20)")
    yield passant, legacy
    passant.close()
    legacy.close()


def test_reference_and_passant_agree_on_simple_scan(paired_rewriters):
    passant, legacy = paired_rewriters
    passant_policy = DFCPolicy(
        sources=["foo"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    legacy_policy = LegacyDFCPolicy(
        sources=["foo"],
        constraint="max(foo.id) > 1",
        on_fail=LegacyResolution.REMOVE,
    )
    passant.register_policy(passant_policy)
    legacy.register_policy(legacy_policy)
    query = "SELECT id FROM foo"
    passant_sql = passant.transform_query(query)
    legacy_sql = legacy.transform_query(query)
    assert (
        passant.conn.execute(passant_sql).fetchall() == legacy.conn.execute(legacy_sql).fetchall()
    )


def test_reference_and_passant_both_execute_join_rewrite(paired_rewriters):
    passant, legacy = paired_rewriters
    passant.register_policy(
        DFCPolicy(sources=["foo"], constraint="max(foo.id) > 1", on_fail=Resolution.REMOVE)
    )
    legacy.register_policy(
        LegacyDFCPolicy(
            sources=["foo"], constraint="max(foo.id) > 1", on_fail=LegacyResolution.REMOVE
        )
    )
    query = "SELECT baz.x FROM baz JOIN foo ON baz.x = foo.id"
    passant_sql = passant.transform_query(query)
    legacy_sql = legacy.transform_query(query)
    assert passant.conn.execute(passant_sql).fetchall() is not None
    assert legacy.conn.execute(legacy_sql).fetchall() is not None

"""Catalog sync tests for schema-qualified and quoted DuckDB identifiers."""

import duckdb

from passant import Policy, Resolution, dfc


def test_catalog_sync_schema_qualified_table():
    rewriter = dfc(duckdb.connect())
    rewriter.execute("CREATE SCHEMA IF NOT EXISTS s")
    rewriter.execute("CREATE OR REPLACE TABLE s.foo (id INTEGER)")
    policy = Policy(
        sources=["s.foo"],
        constraint="max(s.foo.id) > 0",
        on_fail=Resolution.REMOVE,
    )
    rewriter.register_policy(policy)
    assert rewriter.policies() == [policy]


def test_catalog_sync_quoted_table_name():
    rewriter = dfc(duckdb.connect())
    rewriter.execute('CREATE OR REPLACE TABLE "Order" (id INTEGER)')
    policy = Policy(
        sources=["Order"],
        constraint='max("Order".id) > 0',
        on_fail=Resolution.REMOVE,
    )
    rewriter.register_policy(policy)
    assert rewriter.policies() == [policy]

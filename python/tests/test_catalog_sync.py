"""Catalog sync tests for schema-qualified and quoted DuckDB identifiers."""

from passant.compat import DFCPolicy, Resolution, SQLRewriter


def test_catalog_sync_schema_qualified_table():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE SCHEMA IF NOT EXISTS s")
    rewriter.execute("CREATE OR REPLACE TABLE s.foo (id INTEGER)")
    policy = DFCPolicy(
        sources=["s.foo"],
        constraint="max(s.foo.id) > 0",
        on_fail=Resolution.REMOVE,
    )
    rewriter.register_policy(policy)
    assert rewriter.get_dfc_policies() == [policy]


def test_catalog_sync_quoted_table_name():
    rewriter = SQLRewriter()
    rewriter.execute('CREATE OR REPLACE TABLE "Order" (id INTEGER)')
    policy = DFCPolicy(
        sources=["Order"],
        constraint='max("Order".id) > 0',
        on_fail=Resolution.REMOVE,
    )
    rewriter.register_policy(policy)
    assert rewriter.get_dfc_policies() == [policy]

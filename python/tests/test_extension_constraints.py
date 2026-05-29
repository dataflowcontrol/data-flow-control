"""Phase 8: DuckDB extension function calls in CONSTRAINT pass through unchanged."""

from __future__ import annotations

import duckdb
import pytest

from passant import Policy, Resolution, dfc
from flock_support import (
    configure_flock_openai_secret,
    ensure_flock_connection,
    flock_available,
    flock_openai_configured,
)

FLOCK_LLM_FILTER_CONSTRAINT = (
    "llm_filter({'model_name': 'default'}, "
    "{'prompt': 'Does this product description mention explosives?', "
    "'context_columns': [{'data': products.description}]})"
)


def test_custom_scalar_function_in_constraint():
    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE docs (id INTEGER, text VARCHAR)")
    conn.execute("INSERT INTO docs VALUES (1, 'hello')")
    conn.raw_connection.create_function("is_safe", lambda text: text != "secret", [str], bool)
    conn.register_policy(
        Policy(
            sources=["docs"],
            constraint="is_safe(docs.text)",
            on_fail=Resolution.REMOVE,
        )
    )
    rewritten = conn.transform_query("SELECT id, text FROM docs ORDER BY id")
    assert "is_safe(docs.text)" in rewritten
    assert conn.fetchall("SELECT id, text FROM docs ORDER BY id") == [(1, "hello")]


def test_struct_literal_in_constraint_does_not_break_validation():
    conn = dfc(duckdb.connect())
    conn.execute("CREATE TABLE items (id INTEGER, label VARCHAR)")
    conn.raw_connection.create_function(
        "ext_check",
        lambda row: row["label"].startswith("ok"),
        [{"label": str}],
        bool,
    )
    conn.register_policy(
        Policy(
            sources=["items"],
            constraint="ext_check({'label': items.label})",
            on_fail=Resolution.REMOVE,
        )
    )
    rewritten = conn.transform_query("SELECT id, label FROM items ORDER BY id")
    assert "ext_check({'label': items.label})" in rewritten
    conn.execute("INSERT INTO items VALUES (1, 'ok-item')")
    assert conn.fetchall("SELECT id FROM items ORDER BY id") == [(1,)]


@pytest.mark.flock
@pytest.mark.skipif(not flock_available(), reason="Flock DuckDB extension not available")
def test_flock_llm_filter_in_constraint_registers_and_rewrites():
    conn = dfc(duckdb.connect())
    ensure_flock_connection(conn)
    conn.execute("CREATE TABLE products (name VARCHAR, description VARCHAR)")
    conn.execute("INSERT INTO products VALUES ('widget', 'A harmless widget')")
    conn.register_policy(
        Policy(
            sources=["products"],
            constraint=FLOCK_LLM_FILTER_CONSTRAINT,
            on_fail=Resolution.REMOVE,
        )
    )
    rewritten = conn.transform_query("SELECT name, description FROM products ORDER BY name")
    assert "llm_filter" in rewritten
    assert "products.description" in rewritten


@pytest.mark.flock
@pytest.mark.skipif(not flock_available(), reason="Flock DuckDB extension not available")
@pytest.mark.skipif(
    not flock_openai_configured(),
    reason="Set OPENAI_API_KEY or FLOCK_OPENAI_API_KEY to run Flock execution",
)
def test_flock_llm_filter_executes_with_openai_secret():
    conn = dfc(duckdb.connect())
    ensure_flock_connection(conn)
    configure_flock_openai_secret(conn.raw_connection)
    conn.execute("CREATE TABLE products (name VARCHAR, description VARCHAR)")
    conn.execute("INSERT INTO products VALUES ('widget', 'A harmless widget')")
    conn.register_policy(
        Policy(
            sources=["products"],
            constraint=FLOCK_LLM_FILTER_CONSTRAINT,
            on_fail=Resolution.REMOVE,
        )
    )
    rows = conn.fetchall("SELECT name FROM products ORDER BY name")
    assert rows == [("widget",)]

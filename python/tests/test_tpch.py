"""Execution-focused TPC-H regression tests for Passant.

These mirror the legacy TPC-H rewrite test set but assert that each query
rewrites successfully, applies the lineitem policy, and executes against DuckDB.
Exact SQL text is not compared because Passant uses different rewrite strategies
for LIMIT wrappers, join pushdown, and EXISTS handling.

Excluded queries (non-monotonic): Q02, Q11, Q13, Q15, Q16, Q17, Q20, Q21, Q22.
"""

from __future__ import annotations

import pathlib

import duckdb
import pytest

from passant import Connection, Policy, Resolution, wrap

lineitem_policy = Policy(
    sources=["lineitem"],
    constraint="max(lineitem.l_quantity) >= 1",
    on_fail=Resolution.REMOVE,
)

TPCH_QUERIES = (1, 3, 4, 5, 6, 7, 8, 9, 10, 12, 14, 18, 19)


@pytest.fixture
def tpch_rewriter():
    rewriter = wrap(duckdb.connect())
    rewriter.execute("INSTALL tpch")
    rewriter.execute("LOAD tpch")
    rewriter.execute("CALL dbgen(sf=0.1)")
    yield rewriter
    rewriter.close()


def load_tpch_query(query_num: int) -> str:
    benchmarks_dir = pathlib.Path(__file__).resolve().parents[3] / "benchmarks" / "tpch" / "queries"
    query_file = benchmarks_dir / f"q{query_num:02d}.sql"
    if not query_file.exists():
        raise FileNotFoundError(f"TPC-H query {query_num} not found at {query_file}")
    return query_file.read_text()


def assert_tpch_rewrite_executes(
    tpch_rewriter: Connection,
    query_num: int,
    *,
    require_policy_marker: str = "l_quantity",
) -> str:
    query = load_tpch_query(query_num)
    tpch_rewriter.register_policy(lineitem_policy)
    transformed = tpch_rewriter.transform_query(query)
    assert require_policy_marker.lower() in transformed.lower(), transformed
    result = tpch_rewriter.raw_connection.execute(transformed).fetchall()
    assert result is not None
    return transformed


@pytest.mark.parametrize("query_num", [1, 5, 6, 7, 8, 9, 12, 14, 18, 19])
def test_tpch_query_rewrites_and_executes(tpch_rewriter, query_num: int):
    assert_tpch_rewrite_executes(tpch_rewriter, query_num)


@pytest.mark.parametrize("query_num", [3, 10])
def test_tpch_limit_query_rewrites_and_executes(tpch_rewriter, query_num: int):
    """Q3 and Q10 use LIMIT with distributive semiring policies (Full-Push)."""
    transformed = assert_tpch_rewrite_executes(tpch_rewriter, query_num)
    assert "LIMIT" in transformed.upper()
    assert "l_quantity" in transformed.lower()
    assert "base_query" not in transformed.lower()


def test_tpch_q04_exists_query_rewrites_to_join(tpch_rewriter):
    transformed = assert_tpch_rewrite_executes(tpch_rewriter, 4)
    assert "exists_subquery" in transformed.lower()
    assert "base_query" not in transformed.lower()


def test_tpch_q18_limit_aggregation_rewrites_and_executes(tpch_rewriter):
    transformed = assert_tpch_rewrite_executes(tpch_rewriter, 18)
    assert "base_query" not in transformed.lower()
    assert "l_quantity" in transformed.lower()

"""Completion-gated tests mirroring passant-core/tests/completion/.

Run with: uv run pytest -m completion
Default CI excludes these via: uv run pytest -m "not completion"
"""

import duckdb
import pytest

pytestmark = pytest.mark.completion


def test_count_if_scan_rewrites_to_case_when():
    """COUNT_IF scan rewrite (completion gate)."""
    from data_flow_control import Policy, Resolution, dfc

    rewriter = dfc(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.register_policy(
        Policy(
            sources=["foo"],
            constraint="COUNT_IF(foo.id > 2) > 0",
            on_fail=Resolution.REMOVE,
        )
    )
    transformed = rewriter.transform_query("SELECT id FROM foo")
    assert transformed == "SELECT id FROM foo WHERE CASE WHEN foo.id > 2 THEN 1 ELSE 0 END > 0"


def test_delete_policy_removes_registered_policy():
    from data_flow_control import Policy, Resolution, dfc

    rewriter = dfc(duckdb.connect())
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    policy = Policy(
        sources=["foo"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    rewriter.register_policy(policy)
    removed = rewriter.delete_policy(
        sources=["foo"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    assert removed is True
    assert rewriter.policies() == []


def test_pgn_policy_text_parses():
    from data_flow_control import Policy

    policy = Policy.from_pgn(
        "SOURCE foo SINK reports CONSTRAINT sum(foo.amount) <= 1000 ON FAIL REMOVE"
    )
    assert policy.sink == "reports"
    assert policy.constraint == "sum(foo.amount) <= 1000"

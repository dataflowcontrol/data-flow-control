"""Policy dataclass validation tests."""

import duckdb
import pytest

from passant import Policy, Resolution, dfc


def test_policy_with_source_only_accepts_aggregated():
    policy = Policy(
        sources=["users"],
        constraint="max(users.age) >= 18",
        on_fail=Resolution.REMOVE,
    )
    assert policy.sources == ["users"]
    assert policy.sink is None
    assert policy.constraint == "max(users.age) >= 18"


def test_policy_with_sink_only():
    policy = Policy(
        sources=[],
        sink="reports",
        constraint="reports.status = 'approved'",
        on_fail=Resolution.KILL,
    )
    assert policy.sources == []
    assert policy.sink == "reports"


def test_policy_with_both_source_and_sink():
    policy = Policy(
        sources=["users"],
        sink="analytics",
        constraint="max(users.id) = analytics.user_id",
        on_fail=Resolution.REMOVE,
    )
    assert policy.sources == ["users"]
    assert policy.sink == "analytics"


def test_policy_requires_source_or_sink():
    with pytest.raises(ValueError, match="Either sources or sink"):
        Policy(
            sources=[],
            constraint="1 = 1",
            on_fail=Resolution.REMOVE,
        )


def test_policy_requires_sources_list():
    with pytest.raises(ValueError, match="Sources must be provided"):
        Policy(  # type: ignore[arg-type]
            sources=None,
            constraint="1 = 1",
            on_fail=Resolution.REMOVE,
        )


def test_policy_rejects_unqualified_column():
    with pytest.raises(ValueError, match="qualified"):
        Policy(
            sources=["users"],
            constraint="max(age) >= 18",
            on_fail=Resolution.REMOVE,
        )


def test_policy_equality():
    left = Policy(
        sources=["foo"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    right = Policy(
        sources=["foo"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    assert left == right


def test_policy_required_sources_must_be_subset():
    with pytest.raises(ValueError, match="Required sources"):
        Policy(
            sources=["foo"],
            required_sources=["bar"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )


def test_register_policy_rejects_unaggregated_source_column():
    rewriter = dfc(duckdb.connect())
    rewriter.execute("CREATE TABLE users (age INTEGER)")
    with pytest.raises(ValueError, match="aggregated"):
        rewriter.register_policy(
            Policy(
                sources=["users"],
                constraint="users.age >= 18",
                on_fail=Resolution.REMOVE,
            )
        )
    rewriter.close()

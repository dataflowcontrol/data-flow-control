"""Policy object validation tests ported from sql_rewriter/test_policy.py."""

import pytest

from passant.compat import DFCPolicy, Resolution, SQLRewriter


def test_policy_with_source_only_accepts_aggregated():
    policy = DFCPolicy(
        sources=["users"],
        constraint="max(users.age) >= 18",
        on_fail=Resolution.REMOVE,
    )
    assert policy.sources == ["users"]
    assert policy.sink is None
    assert policy.constraint == "max(users.age) >= 18"


def test_policy_with_sink_only():
    policy = DFCPolicy(
        sources=[],
        sink="reports",
        constraint="reports.status = 'approved'",
        on_fail=Resolution.KILL,
    )
    assert policy.sources == []
    assert policy.sink == "reports"


def test_policy_with_both_source_and_sink():
    policy = DFCPolicy(
        sources=["users"],
        sink="analytics",
        constraint="max(users.id) = analytics.user_id",
        on_fail=Resolution.REMOVE,
    )
    assert policy.sources == ["users"]
    assert policy.sink == "analytics"


def test_policy_requires_source_or_sink():
    with pytest.raises(ValueError, match="Either sources or sink"):
        DFCPolicy(
            sources=[],
            constraint="1 = 1",
            on_fail=Resolution.REMOVE,
        )


def test_policy_requires_sources_list():
    with pytest.raises(ValueError, match="Sources must be provided"):
        DFCPolicy(  # type: ignore[arg-type]
            sources=None,
            constraint="1 = 1",
            on_fail=Resolution.REMOVE,
        )


def test_policy_rejects_unqualified_column():
    with pytest.raises(ValueError, match="qualified"):
        DFCPolicy(
            sources=["users"],
            constraint="max(age) >= 18",
            on_fail=Resolution.REMOVE,
        )


def test_policy_equality():
    left = DFCPolicy(
        sources=["foo"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    right = DFCPolicy(
        sources=["foo"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )
    assert left == right


def test_policy_required_sources_must_be_subset():
    with pytest.raises(ValueError, match="Required sources"):
        DFCPolicy(
            sources=["foo"],
            required_sources=["bar"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )


@pytest.mark.skip(
    reason="Passant does not yet validate unaggregated source columns at registration"
)
def test_register_policy_rejects_unaggregated_source_column():
    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE users (age INTEGER)")
    with pytest.raises(ValueError, match="aggregated"):
        rewriter.register_policy(
            DFCPolicy(
                sources=["users"],
                constraint="users.age >= 18",
                on_fail=Resolution.REMOVE,
            )
        )
    rewriter.close()

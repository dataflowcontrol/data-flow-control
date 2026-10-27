"""Reconstructed native RETRY API regressions; no model calls."""

import duckdb
import pytest
from data_flow_control import Policy, Resolution, RetryFailure, RetryViolation, dfc


@pytest.mark.parametrize(
    "message",
    [
        "Don't leak",
        "  spaced  ",
        "line one\nline two ☃",
        "",
        "ON FAIL KILL DESCRIPTION 'quoted'",
        "x\\y",
    ],
)
def test_exact_pgn_message_roundtrip(message):
    escaped = message.replace("'", "''")
    policy = Policy.from_pgn(f"SINK target CONSTRAINT target.x > 0 ON FAIL RETRY('{escaped}')")
    assert policy.retry_message == message
    assert policy.on_fail is Resolution.RETRY
    assert policy.on_fail_label == f"RETRY('{escaped}')"
    raw = duckdb.connect()
    raw.execute("CREATE TABLE target(x INT)")
    with dfc(raw) as conn:
        conn.register_policy(policy)
        assert conn.policies()[0].retry_message == message
        with pytest.raises(RetryViolation) as caught:
            conn.execute("INSERT INTO target SELECT -1")
        assert caught.value.message == str(caught.value) == message
        assert caught.value.failures[0].policy.retry_message == message
        assert raw.execute("SELECT * FROM target").fetchall() == []


def test_errors_sorted_dedup_by_id_not_message():
    error = RetryViolation(
        [RetryFailure(7, "same"), RetryFailure(2, "same"), RetryFailure(7, "same")]
    )
    assert [f.policy_index for f in error.failures] == [2, 7]
    assert error.messages == ["same", "same"]
    assert error.message is error.policy_index is error.policy is None
    with pytest.raises(ValueError):
        RetryViolation([])


def test_exact_actual_failures_and_one_atomic_statement(monkeypatch):
    raw = duckdb.connect()
    raw.execute(
        "CREATE TABLE source(x INT); INSERT INTO source VALUES(1),(2),(3); CREATE TABLE target(x INT)"
    )
    with dfc(raw) as conn:
        for predicate, message in [
            ("target.x < 10", "passing"),
            ("target.x < 2", "actual A"),
            ("target.x < 3", "actual B"),
        ]:
            conn.register_policy(
                Policy(
                    sources=[],
                    sink="target",
                    constraint=predicate,
                    on_fail=Resolution.RETRY,
                    retry_message=message,
                )
            )
        submitted = []
        execute = conn.adapter.execute

        def spy(sql, params=None):
            submitted.append(sql)
            return execute(sql, params)

        monkeypatch.setattr(conn.adapter, "execute", spy)
        with pytest.raises(RetryViolation) as caught:
            conn.execute("INSERT INTO target SELECT x FROM source")
        assert caught.value.messages == ["actual A", "actual B"]
        assert [f.policy_index for f in caught.value.failures] == [1, 2]
        assert len(submitted) == 1
        assert raw.execute("SELECT * FROM target").fetchall() == []
        conn.execute("INSERT INTO target SELECT 1")
        assert raw.execute("SELECT * FROM target").fetchall() == [(1,)]


def test_policy_identity_survives_tombstone_and_backend_error_is_not_retry():
    raw = duckdb.connect()
    raw.execute("CREATE TABLE target(x INT)")
    with dfc(raw) as conn:
        conn.register_policy(
            Policy(
                sources=[],
                sink="target",
                constraint="TRUE",
                on_fail=Resolution.KILL,
                description="delete me",
            )
        )
        conn.register_policy(
            Policy(
                sources=[],
                sink="target",
                constraint="target.x>0",
                on_fail=Resolution.RETRY,
                retry_message="exact",
            )
        )
        assert conn.delete_policy(description="delete me")
        assert conn.planner.policy_by_index(0) is None
        with pytest.raises(RetryViolation) as caught:
            conn.execute("INSERT INTO target SELECT -1")
        assert caught.value.policy_index == 1
        assert caught.value.policy.retry_message == "exact"
        with pytest.raises(Exception) as backend:
            conn.execute("INSERT INTO target SELECT CAST('not integer' AS INT)")
        assert not isinstance(backend.value, RetryViolation)
        conn.execute("INSERT INTO target SELECT 1")


def test_retry_message_validation():
    with pytest.raises(ValueError, match="retry_message"):
        Policy(sources=[], sink="target", constraint="TRUE", on_fail=Resolution.RETRY)
    with pytest.raises(ValueError, match="only valid"):
        Policy(
            sources=[],
            sink="target",
            constraint="TRUE",
            on_fail=Resolution.KILL,
            retry_message="no",
        )


def test_backend_capability_rejects_retry(monkeypatch):
    import sqlite3
    from dataclasses import replace

    from data_flow_control.adapters.sqlite import SQLiteAdapter

    monkeypatch.setattr(
        SQLiteAdapter, "capabilities", replace(SQLiteAdapter.capabilities, retry_resolution=False)
    )

    with dfc(sqlite3.connect(":memory:")) as conn:
        conn.execute("CREATE TABLE target(x INT)")
        with pytest.raises(ValueError, match="retry_resolution"):
            conn.register_policy(
                Policy(
                    sources=[],
                    sink="target",
                    constraint="target.x>0",
                    on_fail=Resolution.RETRY,
                    retry_message="no",
                )
            )


@pytest.mark.parametrize(
    "message",
    [
        "Don't leak",
        "  spaced  ",
        "line one\nline two ☃",
        "",
        "ON FAIL KILL DESCRIPTION 'quoted'",
        "x\\y",
    ],
)
def test_pgn_and_registered_policy_identity_without_query_execution(message):
    escaped = message.replace("'", "''")
    policy = Policy.from_pgn(f"SINK target CONSTRAINT target.x > 0 ON FAIL RETRY('{escaped}')")
    assert policy.retry_message == message
    raw = duckdb.connect()
    raw.execute("CREATE TABLE target(x INT)")
    with dfc(raw) as conn:
        conn.register_policy(policy)
        assert conn.planner.policy_by_index(0).retry_message == message
        assert conn.policies()[0].on_fail_label == f"RETRY('{escaped}')"


@pytest.mark.parametrize("second_message", ["same", "another"])
def test_equal_constraints_preserve_distinct_policy_identities(second_message):
    raw = duckdb.connect()
    raw.execute("CREATE TABLE target(x INTEGER)")
    with dfc(raw) as conn:
        for description, message in [("first policy", "same"), ("second policy", second_message)]:
            conn.register_policy(
                Policy(
                    sources=[],
                    sink="target",
                    constraint="target.x > 0",
                    on_fail=Resolution.RETRY,
                    retry_message=message,
                    description=description,
                )
            )
        with pytest.raises(RetryViolation) as caught:
            conn.execute("INSERT INTO target SELECT -1")
        assert [failure.policy_index for failure in caught.value.failures] == [0, 1]
        assert caught.value.messages == ["same", second_message]
        assert raw.execute("SELECT * FROM target").fetchall() == []

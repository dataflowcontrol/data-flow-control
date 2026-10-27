"""Actual native-engine, actual tool-table dispatch tests; no model calls."""

from __future__ import annotations

import os
import subprocess
import sys
from contextlib import contextmanager
from dataclasses import replace
from pathlib import Path

import pytest
from data_flow_control import Policy, Resolution
from data_flow_control.tools import RetryToolRunner, ToolSpec, ToolUseDatabase

SAFE_SQL = "INSERT INTO SendInput (recipient) SELECT 'safe'"


@pytest.fixture
def db_world():
    world = []

    def send(recipient):
        world.append(recipient)
        return {"sent": recipient}

    db = ToolUseDatabase(
        [ToolSpec("send", "Send", {"recipient": "VARCHAR"}, {"sent": "VARCHAR"}, send, None, None)]
    )
    db.register_policy(
        Policy(
            sources=[],
            sink="SendInput",
            constraint="SendInput.recipient = 'safe'",
            on_fail=Resolution.RETRY,
            retry_message="Use safe recipient",
        )
    )
    yield db, world
    db.close()


def runner(db, **kwargs):
    return RetryToolRunner(db, **kwargs)


def test_ordinary_call_is_guarded_and_safe_call_dispatches_once(db_world):
    db, world = db_world
    result = runner(db).run("send", {"recipient": "safe"})
    assert result["status"] == "executed"
    assert "SELECT 'safe'" in result["attempts"][0]["sql"]
    assert "VALUES" not in result["attempts"][0]["sql"]
    assert world == ["safe"]


def test_policy_mutation_before_call(db_world):
    db, world = db_world
    r = runner(db)
    db.register_policy(
        Policy(sources=[], sink="SendInput", constraint="TRUE", on_fail=Resolution.KILL)
    )
    assert r.run("send", {"recipient": "safe"})["status"] == "policy_changed"
    assert world == []


def test_policy_binder_failure_is_not_action_retry(db_world, monkeypatch):
    import duckdb

    db, world = db_world

    def broken(_sql):
        raise duckdb.BinderException("invalid configured policy column")

    monkeypatch.setattr(db.conn, "execute", broken)
    assert runner(db).run("send", {"recipient": "safe"})["status"] == "engine_error"
    assert world == []


def test_local_simulator_failure_rolls_back_world_and_io(db_world, monkeypatch):
    db, world = db_world

    def failing(**kwargs):
        world.append(kwargs)
        raise RuntimeError("Tool failed after local mutation")

    db.spec_by_name["send"] = replace(db.spec_by_name["send"], call_sync=failing)

    @contextmanager
    def atomic_world():
        snapshot = list(world)
        try:
            yield
        except Exception:
            world[:] = snapshot
            raise

    result = runner(db, action_transaction=atomic_world).run("send", {"recipient": "safe"})
    assert result["status"] == "tool_error"
    assert result["result"]["external_outcome"] == "rolled_back"
    assert world == []
    assert db.fetchall("SELECT * FROM SendInput") == []
    assert db.fetchall("SELECT * FROM SendOutput") == []
    assert db.fetchall("SELECT tool_calls_executed FROM __passant_tool_sql_calls") == [(1,)]


def test_remote_tool_failure_is_terminal_and_retains_attempt_log(db_world):
    db, world = db_world

    def failing(recipient):
        world.append(recipient)
        raise RuntimeError("Remote outcome uncertain")

    db.spec_by_name["send"] = replace(db.spec_by_name["send"], call_sync=failing)
    result = runner(db).run("send", {"recipient": "safe"})
    assert result["status"] == "tool_error"
    assert world == ["safe"]
    assert db.fetchall("SELECT __passant_status FROM SendInput") == [("failed",)]
    assert db.fetchall("SELECT __passant_status FROM SendOutput") == [("failed",)]
    assert len(result["attempts"]) == 1


def test_audit_failure_after_effect_is_terminal(db_world, monkeypatch):
    db, world = db_world

    def failing_log(**kwargs):
        raise RuntimeError("disk failure")

    monkeypatch.setattr(db, "log_sql_call", failing_log)
    result = runner(db).run("send", {"recipient": "safe"})
    assert result["status"] == "audit_error"
    assert result["result"]["external_outcome"] == "uncertain"
    assert world == ["safe"]
    assert len(result["attempts"]) == 1


def test_audit_failure_rolls_back_local_simulator(db_world, monkeypatch):
    db, world = db_world

    def failing_log(**kwargs):
        raise RuntimeError("disk failure")

    @contextmanager
    def atomic_world():
        snapshot = list(world)
        try:
            yield
        except Exception:
            world[:] = snapshot
            raise

    monkeypatch.setattr(db, "log_sql_call", failing_log)
    result = runner(db, action_transaction=atomic_world).run("send", {"recipient": "safe"})
    assert result["status"] == "audit_error"
    assert world == []
    assert db.fetchall("SELECT * FROM SendInput") == []
    assert db.fetchall("SELECT * FROM SendOutput") == []


def test_no_langchain_dependency_and_legacy_module_identity():
    script = """
import importlib.abc
import sys
class DenyLangchain(importlib.abc.MetaPathFinder):
    def find_spec(self, fullname, path=None, target=None):
        if fullname == "langchain" or fullname.startswith("langchain.") or fullname == "langchain_core" or fullname.startswith("langchain_core."):
            raise ModuleNotFoundError("intentionally unavailable", name=fullname)
sys.meta_path.insert(0, DenyLangchain())
from data_flow_control.tools import ToolUseDatabase, ToolSpec, RetryToolRunner
from data_flow_control.langchain.database import ToolUseDatabase as LegacyDatabase
assert ToolUseDatabase is LegacyDatabase
assert not any(k == "langchain" or k.startswith("langchain.") for k in sys.modules)
"""
    env = dict(os.environ, PYTHONPATH=str(Path(__file__).resolve().parents[1]))
    subprocess.run([sys.executable, "-c", script], check=True, env=env)


def test_dispatch_preserves_checked_null_instead_of_callable_default():
    world = []

    def send(recipient="unsafe_default"):
        world.append(recipient)

    db = ToolUseDatabase(
        [ToolSpec("send", "", {"recipient": "VARCHAR"}, {}, send, None, None)],
        policies=[
            Policy(
                sources=[],
                sink="SendInput",
                constraint="TRUE",
                on_fail=Resolution.KILL,
            )
        ],
    )
    try:
        assert runner(db).run("send", {"recipient": None})["status"] == "executed"
        assert world == [None]
    finally:
        db.close()


def test_omitted_default_is_materialized_before_guarding_and_explicit_none_is_distinct():
    world = []

    def send(recipient="safe_default"):
        world.append(recipient)

    db = ToolUseDatabase(
        [ToolSpec("send", "", {"recipient": "VARCHAR"}, {}, send, None, None)],
        policies=[Policy(sources=[], sink="SendInput", constraint="TRUE", on_fail=Resolution.KILL)],
    )
    try:
        r = runner(db)
        omitted = r.run("send", {})
        explicit = r.run("send", {"recipient": None})
        assert omitted["status"] == explicit["status"] == "executed"
        assert "'safe_default'" in omitted["attempts"][0]["sql"]
        assert "SELECT NULL" in explicit["attempts"][0]["sql"]
        assert world == ["safe_default", None]
        assert db.fetchall("SELECT recipient FROM SendInput ORDER BY __passant_input_id") == [
            ("safe_default",),
            (None,),
        ]
    finally:
        db.close()


def test_unsafe_default_cannot_bypass_guard():
    world = []

    def send(recipient="unsafe_default"):
        world.append(recipient)

    db = ToolUseDatabase(
        [ToolSpec("send", "", {"recipient": "VARCHAR"}, {}, send, None, None)],
        policies=[
            Policy(
                sources=[],
                sink="SendInput",
                constraint="SendInput.recipient = 'safe'",
                on_fail=Resolution.KILL,
            )
        ],
    )
    try:
        assert RetryToolRunner(db).run("send", {})["status"] == "hard_stop"
        assert world == []
    finally:
        db.close()


def test_hook_exit_failure_after_commit_is_uncertain_and_does_not_duplicate_audit(db_world):
    db, world = db_world

    @contextmanager
    def broken_exit():
        yield
        raise RuntimeError("exit failed after commit")

    result = runner(db, action_transaction=broken_exit).run("send", {"recipient": "safe"})
    assert result["status"] == "action_error"
    assert result["result"]["external_outcome"] == "uncertain"
    assert result["result"]["database_committed"] is True
    assert world == ["safe"]
    assert db.fetchall("SELECT __passant_status FROM SendInput") == [("succeeded",)]
    assert db.fetchall("SELECT COUNT(*) FROM __passant_tool_sql_calls") == [(1,)]
    assert "exit failed" in db.fetchall("SELECT error FROM __passant_tool_sql_calls")[0][0]


def test_suppressing_hook_does_not_commit_or_claim_rollback(db_world):
    db, world = db_world

    def failing(recipient):
        world.append(recipient)
        raise RuntimeError("tool failed")

    @contextmanager
    def suppress_failure():
        try:
            yield
        except Exception:
            pass

    db.spec_by_name["send"] = replace(db.spec_by_name["send"], call_sync=failing)
    result = runner(db, action_transaction=suppress_failure).run("send", {"recipient": "safe"})
    assert result["status"] == "action_error"
    assert result["result"]["external_outcome"] == "uncertain"
    assert result["result"]["database_committed"] is False
    assert db.fetchall("SELECT * FROM SendInput") == []
    assert world == ["safe"]


def test_exact_failure_list_and_explicit_host_replacement(db_world):
    db, world = db_world
    db.register_policy(
        Policy.from_pgn(
            "SINK SendInput CONSTRAINT TRUE ON FAIL RETRY('Passing message must not appear')"
        )
    )
    db.register_policy(
        Policy.from_pgn(
            "SINK SendInput CONSTRAINT SendInput.recipient <> 'bad' ON FAIL RETRY('Second actual failure')"
        )
    )
    r = runner(db)
    result = r.run("send", {"recipient": "bad"})
    assert result["status"] == "retry"
    assert result["message"] is None
    assert result["messages"] == ["Use safe recipient", "Second actual failure"]
    assert [f["policy_index"] for f in result["failures"]] == [0, 2]
    assert "Passing message must not appear" not in str(result)
    assert len(result["attempts"]) == 1
    assert world == []
    assert db.fetchall("SELECT * FROM SendInput") == []
    assert r.run_sql("send", SAFE_SQL)["status"] == "executed"
    assert world == ["safe"]


def test_proposed_query_submitted_once_without_candidate_preflight(db_world, monkeypatch):
    db, world = db_world
    raw_calls, checked_calls = [], []
    raw_execute, checked_execute = db._execute_raw, db.conn.execute

    def raw(sql, *args, **kwargs):
        raw_calls.append(sql)
        return raw_execute(sql, *args, **kwargs)

    def checked(sql, *args, **kwargs):
        checked_calls.append(sql)
        return checked_execute(sql, *args, **kwargs)

    monkeypatch.setattr(db, "_execute_raw", raw)
    monkeypatch.setattr(db.conn, "execute", checked)
    assert runner(db).run_sql("send", SAFE_SQL)["status"] == "executed"
    assert checked_calls == [SAFE_SQL]
    assert "SELECT 'safe'" not in raw_calls
    assert world == ["safe"]


@pytest.mark.parametrize("values", [("safe", "bad"), ("safe", "safe")])
def test_multiple_rows_never_dispatch_and_leave_no_partial_input(db_world, values):
    db, world = db_world
    db._execute_raw("CREATE TABLE Observed (recipient VARCHAR)")
    db._execute_raw("INSERT INTO Observed VALUES (?), (?)", list(values))
    result = runner(db, readable_tables=["Observed"]).run_sql(
        "send", "INSERT INTO SendInput (recipient) SELECT recipient FROM Observed"
    )
    assert result["status"] == ("retry" if "bad" in values else "invalid_sql")
    assert world == []
    assert db.fetchall("SELECT * FROM SendInput") == []


@pytest.mark.parametrize(
    "sql",
    [
        "INSERT INTO SendInput (recipient) VALUES ('safe')",
        "INSERT INTO main.SendInput (recipient) SELECT 'safe'",
        "INSERT INTO OtherInput (recipient) SELECT 'safe'",
        "INSERT INTO SendInput (__PASSANT_STATUS) SELECT 'safe'",
        "INSERT INTO SendInput (recipient) SELECT read_text('/etc/passwd')",
        "INSERT INTO SendInput (recipient) SELECT recipient FROM hidden",
        "INSERT INTO SendInput (recipient) SELECT 'safe' RETURNING *",
        "INSERT OR REPLACE INTO SendInput (recipient) SELECT 'safe'",
        "SELECT 'safe'",
        "INSERT INTO SendInput (recipient) SELECT 'safe'; DELETE FROM SendInput",
    ],
)
def test_unsafe_sql_never_dispatches(db_world, sql):
    db, world = db_world
    assert runner(db).run_sql("send", sql)["status"] == "invalid_sql"
    assert world == []


def test_zero_rows_and_backend_failure_are_not_retry(db_world):
    db, world = db_world
    r = runner(db)
    assert (
        r.run_sql("send", "INSERT INTO SendInput (recipient) SELECT 'safe' WHERE FALSE")["status"]
        == "no_action"
    )
    assert (
        r.run_sql("send", "INSERT INTO SendInput (recipient) SELECT missing_column")["status"]
        == "engine_error"
    )
    assert world == []


def test_nested_user_transaction_is_unsupported_without_implicit_rollback(db_world, monkeypatch):
    db, world = db_world
    r = runner(db)
    db._execute_raw("CREATE TABLE UserWork (value INTEGER)")
    db._execute_raw("BEGIN TRANSACTION")
    db._execute_raw("INSERT INTO UserWork VALUES (7)")
    commands = []
    execute = db._execute_raw

    def record(sql, *args, **kwargs):
        commands.append(sql)
        return execute(sql, *args, **kwargs)

    monkeypatch.setattr(db, "_execute_raw", record)
    result = r.run("send", {"recipient": "bad"})
    assert result["status"] in {"engine_error", "audit_error"}
    assert world == []
    assert "ROLLBACK" not in commands and "COMMIT" not in commands
    # DuckDB itself aborts on nested BEGIN. The runner offers no savepoints.
    db._execute_raw("COMMIT")
    assert db._execute_raw("SELECT * FROM UserWork").fetchall() == []

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
from data_flow_control.tools import RetryDirective, RetryToolRunner, ToolSpec, ToolUseDatabase

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
            on_fail=Resolution.KILL,
        )
    )
    yield db, world
    db.close()


def runner(db, **kwargs):
    return RetryToolRunner(
        db,
        directives=[RetryDirective.for_policy(p, "Use safe recipient") for p in db.conn.policies()],
        **kwargs,
    )


def never(_request):
    pytest.fail("No corrective callback expected")


def test_ordinary_call_is_guarded_and_safe_call_dispatches_once(db_world):
    db, world = db_world
    result = runner(db).run("send", {"recipient": "safe"}, never)
    assert result["status"] == "executed"
    assert "SELECT 'safe'" in result["attempts"][0]["sql"]
    assert "VALUES" not in result["attempts"][0]["sql"]
    assert world == ["safe"]


def test_recovery_logs_rejected_attempt_then_dispatches_checked_arguments(db_world):
    db, world = db_world
    seen = []

    def turn(request):
        assert world == []
        assert db.fetchall("SELECT * FROM SendInput") == []
        seen.append(request)
        return {"sql": SAFE_SQL}

    result = runner(db).run("send", {"recipient": "bad"}, turn)
    assert result["status"] == "recovered"
    assert world == ["safe"]
    assert len(seen) == 1
    feedback = seen[0]["feedback"]
    assert feedback["context"] == {}
    assert feedback["readable_source_schemas"] == {}
    assert feedback["applicable_policies"][0]["retry_message"] == "Use safe recipient"
    assert "not identified failed" in feedback["violation_attribution"]
    assert db.fetchall("SELECT __passant_status FROM SendInput") == [("succeeded",)]
    assert db.fetchall(
        "SELECT tool_calls_executed FROM __passant_tool_sql_calls ORDER BY sql_call_id"
    ) == [(0,), (1,)]


def test_unmarked_kill_stays_hard_stop(db_world):
    db, world = db_world
    result = RetryToolRunner(db).run("send", {"recipient": "bad"}, never)
    assert result["status"] == "hard_stop"
    assert world == []


def test_all_applicable_policies_must_opt_in(db_world):
    db, world = db_world
    directive = RetryDirective.for_policy(db.conn.policies()[0], "Use safe")
    db.register_policy(
        Policy(sources=[], sink="SendInput", constraint="TRUE", on_fail=Resolution.KILL)
    )
    result = RetryToolRunner(db, directives=[directive]).run("send", {"recipient": "bad"}, never)
    assert result["status"] == "hard_stop"
    assert world == []


def test_exhaustion_never_dispatches(db_world):
    db, world = db_world
    result = runner(db, max_retries=2).run(
        "send",
        {"recipient": "bad"},
        lambda _: {"sql": "INSERT INTO SendInput (recipient) SELECT 'bad'"},
    )
    assert result["status"] == "exhausted"
    assert len(result["turns"]) == 2
    assert len(result["attempts"]) == 3
    assert world == []
    assert db.fetchall("SELECT * FROM SendInput") == []
    assert db.fetchall("SELECT COUNT(*) FROM __passant_tool_sql_calls") == [(3,)]


def test_zero_budget(db_world):
    db, world = db_world
    assert (
        runner(db, max_retries=0).run("send", {"recipient": "bad"}, never)["status"] == "exhausted"
    )
    assert world == []


def test_true_zero_candidate_is_not_filtered_candidate(db_world):
    db, world = db_world
    result = runner(db).run(
        "send", {"recipient": "bad"}, lambda _: {"sql": SAFE_SQL + " WHERE FALSE"}
    )
    assert result["status"] == "no_action"
    assert result["attempts"][0]["status"] == "policy_denial"
    assert result["attempts"][0]["candidate_count"] == 1
    assert result["attempts"][1]["candidate_count"] == 0
    assert result["attempts"][1]["inserted_count"] == 0
    assert world == []


def test_remove_filter_remains_terminal_not_retry():
    world = []
    db = ToolUseDatabase(
        [
            ToolSpec(
                "send",
                "",
                {"recipient": "VARCHAR"},
                {},
                lambda **args: world.append(args),
                None,
                None,
            )
        ],
        policies=[
            Policy(sources=[], sink="SendInput", constraint="FALSE", on_fail=Resolution.REMOVE)
        ],
    )
    try:
        result = RetryToolRunner(db).run("send", {"recipient": "bad"}, never)
        assert result["status"] == "filtered"
        assert result["attempts"][0]["candidate_count"] == 1
        assert world == []
        with pytest.raises(ValueError, match="KILL"):
            runner(db)
    finally:
        db.close()


def test_policy_mutation_stops_before_next_attempt(db_world):
    db, world = db_world

    def turn(_request):
        db.register_policy(
            Policy(sources=[], sink="SendInput", constraint="TRUE", on_fail=Resolution.KILL)
        )
        return {"sql": SAFE_SQL}

    result = runner(db).run("send", {"recipient": "bad"}, turn)
    assert result["status"] == "policy_changed"
    assert len(result["attempts"]) == 1
    assert world == []


def test_policy_mutation_before_call(db_world):
    db, world = db_world
    r = runner(db)
    db.register_policy(
        Policy(sources=[], sink="SendInput", constraint="TRUE", on_fail=Resolution.KILL)
    )
    assert r.run("send", {"recipient": "safe"}, never)["status"] == "policy_changed"
    assert world == []


def test_runtime_binder_error_is_terminal_engine_error(db_world):
    db, world = db_world
    result = runner(db).run(
        "send",
        {"recipient": "bad"},
        lambda _: {"sql": "INSERT INTO SendInput (recipient) SELECT missing_column"},
    )
    assert result["status"] == "engine_error"
    assert len(result["turns"]) == 1
    assert world == []


def test_policy_binder_failure_is_not_action_retry(db_world, monkeypatch):
    import duckdb

    db, world = db_world

    def broken(_sql):
        raise duckdb.BinderException("invalid configured policy column")

    monkeypatch.setattr(db.conn, "execute", broken)
    assert runner(db).run("send", {"recipient": "safe"}, never)["status"] == "engine_error"
    assert world == []


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
def test_unsafe_sql_is_bounded_and_never_dispatches(db_world, sql):
    db, world = db_world
    result = runner(db, max_retries=1).run("send", {"recipient": "bad"}, lambda _: {"sql": sql})
    assert result["status"] == "exhausted"
    assert result["attempts"][-1]["status"] == "invalid_sql"
    assert world == []


def test_invalid_sql_can_receive_separate_bounded_feedback(db_world):
    db, world = db_world
    replies = iter(["not sql", SAFE_SQL])
    categories = []

    def turn(request):
        categories.append(request["feedback"]["category"])
        return {"sql": next(replies)}

    result = runner(db).run("send", {"recipient": "bad"}, turn)
    assert result["status"] == "recovered"
    assert categories == ["policy_denial", "invalid_sql"]
    assert world == ["safe"]


def test_required_source_checks_real_sql_provenance():
    world = []
    db = ToolUseDatabase(
        [
            ToolSpec(
                "send",
                "",
                {"recipient": "VARCHAR"},
                {},
                lambda **args: world.append(args),
                None,
                None,
            )
        ]
    )
    try:
        db._execute_raw("CREATE TABLE Observed (recipient VARCHAR)")
        db._execute_raw("INSERT INTO Observed VALUES ('safe')")
        db.register_policy(
            Policy(
                sources=["Observed"],
                required_sources=["Observed"],
                sink="SendInput",
                constraint="max(Observed.recipient) = SendInput.recipient",
                on_fail=Resolution.KILL,
            )
        )
        r = runner(db, readable_tables=["Observed"])
        seen = []

        def turn(request):
            seen.append(request)
            return {"sql": "INSERT INTO SendInput (recipient) SELECT recipient FROM Observed"}

        result = r.run("send", {"recipient": "safe"}, turn, context={"already_seen": "safe"})
        assert result["status"] == "recovered"
        assert result["attempts"][0]["status"] == "policy_denial"
        assert world == [{"recipient": "safe"}]
        assert seen[0]["feedback"]["context"] == {"already_seen": "safe"}
        assert seen[0]["feedback"]["readable_source_schemas"] == {
            "Observed": [{"name": "recipient", "type": "VARCHAR"}]
        }
    finally:
        db.close()


def test_coverage_gap_is_not_policy_violation():
    world = []
    db = ToolUseDatabase(
        [
            ToolSpec(
                "send",
                "",
                {"recipient": "VARCHAR"},
                {},
                lambda **args: world.append(args),
                None,
                None,
            )
        ]
    )
    try:
        assert (
            RetryToolRunner(db).run("send", {"recipient": "safe"}, never)["status"]
            == "coverage_gap"
        )
        assert world == []
        assert (
            RetryToolRunner(db, require_sink_policy=False).run(
                "send", {"recipient": "safe"}, never
            )["status"]
            == "executed"
        )
    finally:
        db.close()


def test_fanout_not_dispatched(db_world):
    db, world = db_world
    db._execute_raw("CREATE TABLE Observed (recipient VARCHAR)")
    db._execute_raw("INSERT INTO Observed VALUES ('safe'), ('safe')")
    result = runner(db, readable_tables=["Observed"], max_retries=1).run(
        "send",
        {"recipient": "bad"},
        lambda _: {"sql": "INSERT INTO SendInput (recipient) SELECT recipient FROM Observed"},
    )
    assert result["status"] == "exhausted"
    assert result["attempts"][-1]["candidate_count"] == 2
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

    result = runner(db, action_transaction=atomic_world).run("send", {"recipient": "safe"}, never)
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
    result = runner(db).run("send", {"recipient": "safe"}, never)
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
    result = runner(db).run("send", {"recipient": "safe"}, never)
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
    result = runner(db, action_transaction=atomic_world).run("send", {"recipient": "safe"}, never)
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


def test_bad_callback_result_and_callback_error_are_terminal(db_world):
    db, world = db_world
    assert (
        runner(db).run("send", {"recipient": "bad"}, lambda _: {"tool": "send"})["status"]
        == "protocol_error"
    )

    def broken(_):
        raise RuntimeError("transport failed")

    assert runner(db).run("send", {"recipient": "bad"}, broken)["status"] == "callback_error"
    assert world == []


@pytest.mark.parametrize("value", [-1, True, 1.5])
def test_invalid_retry_budget(db_world, value):
    db, _ = db_world
    with pytest.raises(ValueError, match="nonnegative integer"):
        runner(db, max_retries=value)


def test_context_bounds_and_escaping(db_world):
    db, world = db_world
    result = runner(db, max_context_chars=1).run(
        "send", {"recipient": "safe"}, never, context={"x": "y"}
    )
    assert result["status"] == "invalid_request"
    assert world == []
    result = RetryToolRunner(db).run(
        "send", {"recipient": "safe'); DROP TABLE SendInput; --"}, never
    )
    assert result["status"] == "hard_stop"
    assert db.fetchall("SELECT COUNT(*) FROM SendInput") == [(0,)]


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
        assert runner(db).run("send", {"recipient": None}, never)["status"] == "executed"
        assert world == [None]
    finally:
        db.close()


@pytest.mark.parametrize("use_retry", [False, True])
def test_semicolon_in_string_is_not_multiple_statements(use_retry):
    world = []
    db = ToolUseDatabase(
        [
            ToolSpec(
                "send",
                "",
                {"recipient": "VARCHAR"},
                {},
                lambda **kwargs: world.append(kwargs),
                None,
                None,
            )
        ],
        policies=[
            Policy(
                sources=[],
                sink="SendInput",
                constraint="SendInput.recipient = 'safe; ordinary text'",
                on_fail=Resolution.KILL,
            )
        ],
    )
    try:
        result = runner(db).run(
            "send",
            {"recipient": "bad" if use_retry else "safe; ordinary text"},
            lambda _: {"sql": "INSERT INTO SendInput (recipient) SELECT 'safe; ordinary text'"},
        )
        assert result["status"] == ("recovered" if use_retry else "executed")
        assert world == [{"recipient": "safe; ordinary text"}]
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
        omitted = r.run("send", {}, never)
        explicit = r.run("send", {"recipient": None}, never)
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
        assert RetryToolRunner(db).run("send", {}, never)["status"] == "hard_stop"
        assert world == []
    finally:
        db.close()


def test_retry_must_materialize_all_columns():
    world = []

    def send(recipient, body="safe"):
        world.append((recipient, body))

    db = ToolUseDatabase(
        [ToolSpec("send", "", {"recipient": "VARCHAR", "body": "VARCHAR"}, {}, send, None, None)],
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
        result = runner(db, max_retries=1).run(
            "send", {"recipient": "bad"}, lambda _: {"sql": SAFE_SQL}
        )
        assert result["status"] == "exhausted"
        assert result["attempts"][-1]["status"] == "invalid_sql"
        assert world == []
    finally:
        db.close()


def test_hook_exit_failure_after_commit_is_uncertain_and_does_not_duplicate_audit(db_world):
    db, world = db_world

    @contextmanager
    def broken_exit():
        yield
        raise RuntimeError("exit failed after commit")

    result = runner(db, action_transaction=broken_exit).run("send", {"recipient": "safe"}, never)
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
    result = runner(db, action_transaction=suppress_failure).run(
        "send", {"recipient": "safe"}, never
    )
    assert result["status"] == "action_error"
    assert result["result"]["external_outcome"] == "uncertain"
    assert result["result"]["database_committed"] is False
    assert db.fetchall("SELECT * FROM SendInput") == []
    assert world == ["safe"]

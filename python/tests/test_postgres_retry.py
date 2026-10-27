"""PostgreSQL structured RETRY transport and opt-in real-engine conformance."""

import json
from types import SimpleNamespace
from unittest.mock import Mock

import pytest
from data_flow_control import Policy, Resolution, RetryViolation, dfc
from data_flow_control.adapters import postgres
from data_flow_control.catalog import build_catalog_snapshot
from data_flow_control.errors import _NativeRetrySignal
from data_flow_control.planner import Planner


class _ServerError(Exception):
    def __init__(self, sqlstate, detail, message="untrusted display text"):
        super().__init__(message)
        self.sqlstate = sqlstate
        self.diag = SimpleNamespace(message_detail=detail)


@pytest.fixture
def fake_adapter(monkeypatch):
    monkeypatch.setattr(postgres, "psycopg", SimpleNamespace(Error=_ServerError))
    raw = Mock()
    raw.autocommit = False
    return postgres.PostgresAdapter(raw), raw


def _detail(ids="4,2,4", kind="dfc.retry.v1"):
    return json.dumps({"kind": kind, "policy_ids": ids})


def test_structured_retry_sorted_deduped_and_connection_reusable(fake_adapter):
    adapter, raw = fake_adapter
    original = _ServerError("PDR01", _detail())
    raw.cursor.return_value.execute.side_effect = [original, None, ValueError("ordinary")]
    with pytest.raises(_NativeRetrySignal) as caught:
        adapter.execute("SELECT candidate", (1,))
    assert caught.value.failures == ((2, None), (4, None))
    assert caught.value.__cause__ is original
    raw.rollback.assert_called_once()
    raw.cursor.return_value.execute.assert_called_once_with("SELECT candidate", (1,))
    assert adapter.execute("SELECT success") is raw.cursor.return_value
    with pytest.raises(ValueError, match="ordinary"):
        adapter.execute("SELECT unrelated")
    assert raw.rollback.call_count == 2


@pytest.mark.parametrize(
    "state,detail",
    [
        ("P0001", _detail()),
        ("PDR01", None),
        ("PDR01", "Native RETRY policy signal"),
        ("PDR01", "[]"),
        ("PDR01", _detail(kind="unrelated")),
        *[("PDR01", _detail(ids)) for ids in ("", "0,", "-1", "x", "１", 1, None)],
    ],
)
def test_unrelated_or_malformed_diagnostic_is_not_retry(fake_adapter, state, detail):
    adapter, raw = fake_adapter
    original = _ServerError(state, detail, message="Native RETRY policy signal")
    raw.cursor.return_value.execute.side_effect = original
    with pytest.raises(_ServerError) as caught:
        adapter.execute("SELECT query")
    assert caught.value is original
    raw.rollback.assert_called_once()


def test_installation_matches_existing_kill_transaction_contract(fake_adapter):
    adapter, raw = fake_adapter
    adapter.register_retry_function()
    statement = raw.cursor.return_value.execute.call_args.args[0]
    assert "CREATE OR REPLACE FUNCTION passant_retry_many(policy_ids text)" in statement
    assert "LANGUAGE plpgsql VOLATILE" in statement
    assert "ERRCODE = 'PDR01'" in statement
    assert "DETAIL = json_build_object" in statement
    raw.commit.assert_called_once()
    raw.reset_mock()
    raw.autocommit = True
    adapter.register_retry_function()
    raw.commit.assert_not_called()


def test_postgres_rewrite_uses_id_payload_not_authored_messages():
    planner = Planner(dialect="postgres")
    planner.sync_catalog(
        build_catalog_snapshot(
            dialect="postgres",
            tables={
                name: {"columns": ["x"], "types": {"x": "INTEGER"}} for name in ("source", "target")
            },
        )
    )
    for predicate in ("target.x < 2", "target.x < 3"):
        planner.register_policy(
            Policy(
                sources=[],
                sink="target",
                constraint=predicate,
                on_fail=Resolution.RETRY,
                retry_message="  exact authored ☃ message  ",
            )
        )
    sql = planner.rewrite("INSERT INTO target SELECT x FROM source")
    assert "passant_retry_many(concat_ws(" in sql
    assert "THEN '0'" in sql and "THEN '1'" in sql
    assert "exact authored" not in sql


@pytest.mark.postgres
def test_postgres_retry_real_transport(passant_docker):
    psycopg = pytest.importorskip("psycopg")
    raw = psycopg.connect(passant_docker.postgres_url, autocommit=True)
    adapter = postgres.PostgresAdapter(raw)
    try:
        adapter.register_retry_function()
        adapter.register_retry_function()
        with pytest.raises(_NativeRetrySignal) as caught:
            adapter.execute("SELECT passant_retry_many(%s)", ("4,2,4",))
        assert caught.value.failures == ((2, None), (4, None))
        assert adapter.execute("SELECT 1").fetchone() == (1,)
        with pytest.raises(psycopg.Error):
            adapter.execute("SELECT passant_retry_many(%s)", ("-1",))
        with pytest.raises(psycopg.Error):
            adapter.execute("SELECT CAST('Native RETRY policy signal' AS integer)")
    finally:
        raw.close()


@pytest.mark.postgres
def test_postgres_retry_all_failures_atomic_insert_and_reuse(passant_docker, monkeypatch):
    psycopg = pytest.importorskip("psycopg")
    raw = psycopg.connect(passant_docker.postgres_url, autocommit=True)
    try:
        raw.execute("DROP TABLE IF EXISTS passant_retry_pg_source, passant_retry_pg_target")
        raw.execute("CREATE TABLE passant_retry_pg_source(x integer)")
        raw.execute("INSERT INTO passant_retry_pg_source VALUES (1), (2), (3)")
        raw.execute("CREATE TABLE passant_retry_pg_target(x integer)")
        conn = dfc(raw, dialect="postgres")
        for predicate, message in [
            ("passant_retry_pg_target.x < 10", "passing"),
            ("passant_retry_pg_target.x < 2", "  duplicate ☃\nmessage  "),
            ("passant_retry_pg_target.x < 3", "  duplicate ☃\nmessage  "),
        ]:
            conn.register_policy(
                Policy(
                    sources=[],
                    sink="passant_retry_pg_target",
                    constraint=predicate,
                    on_fail=Resolution.RETRY,
                    retry_message=message,
                )
            )
        # Catalog acquisition is not a candidate evaluation; count actual writes only.
        submitted = []
        execute = conn.adapter.execute

        def spy(sql, params=None):
            if sql.lstrip().upper().startswith("INSERT"):
                submitted.append(sql)
            return execute(sql, params)

        monkeypatch.setattr(conn.adapter, "execute", spy)
        with pytest.raises(RetryViolation) as caught:
            conn.execute(
                "INSERT INTO passant_retry_pg_target SELECT x FROM passant_retry_pg_source"
            )
        assert [failure.policy_index for failure in caught.value.failures] == [1, 2]
        assert caught.value.messages == ["  duplicate ☃\nmessage  "] * 2
        assert len(submitted) == 1
        assert raw.execute("SELECT * FROM passant_retry_pg_target").fetchall() == []
        conn.execute("INSERT INTO passant_retry_pg_target SELECT 1 AS x")
        assert raw.execute("SELECT * FROM passant_retry_pg_target").fetchall() == [(1,)]
        conn.execute(
            "INSERT INTO passant_retry_pg_target SELECT x FROM passant_retry_pg_source WHERE false"
        )
        with pytest.raises(psycopg.Error):
            conn.execute(
                "INSERT INTO passant_retry_pg_target SELECT CAST('invalid' AS integer) AS x"
            )
        assert raw.execute("SELECT * FROM passant_retry_pg_target").fetchall() == [(1,)]
    finally:
        raw.execute("DROP TABLE IF EXISTS passant_retry_pg_source, passant_retry_pg_target")
        raw.close()

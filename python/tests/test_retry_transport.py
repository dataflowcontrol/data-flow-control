"""Execution-local callback evidence, never exception-text recognition."""

from threading import Event, Thread

import duckdb
import pytest
from data_flow_control import Policy, Resolution, RetryViolation, dfc
from data_flow_control.adapters.duckdb import DuckDBAdapter
from data_flow_control.errors import _NativeRetrySignal


def test_shared_raw_connection_has_shared_lock_and_transport_state():
    raw = duckdb.connect()
    one, two = DuckDBAdapter(raw), DuckDBAdapter(raw)
    one.register_retry_function()
    two.register_retry_function()
    with pytest.raises(_NativeRetrySignal) as caught:
        one.execute("SELECT passant_retry_many('4,2,4')")
    assert caught.value.failures == ((2, None), (4, None))
    assert two.execute("SELECT 1").fetchall() == [(1,)]
    with pytest.raises(duckdb.Error) as caught:
        two.execute("SELECT missing_column")
    assert not isinstance(caught.value, _NativeRetrySignal)
    raw.close()


def test_invalid_private_callback_payload_does_not_forge_retry():
    raw = duckdb.connect()
    adapter = DuckDBAdapter(raw)
    adapter.register_retry_function()
    for payload in ("", "0,", "-1", "not_an_id"):
        with pytest.raises(duckdb.Error):
            adapter.execute("SELECT passant_retry_many(?)", [payload])
    assert adapter.execute("SELECT 1").fetchone() == (1,)
    raw.close()


def test_independent_connections_do_not_share_failure_evidence():
    first, second = duckdb.connect(), duckdb.connect()
    a, b = DuckDBAdapter(first), DuckDBAdapter(second)
    a.register_retry_function()
    b.register_retry_function()
    with pytest.raises(_NativeRetrySignal):
        a.execute("SELECT passant_retry_many('9')")
    assert b.execute("SELECT 2").fetchone() == (2,)
    with pytest.raises(duckdb.Error):
        b.execute("SELECT CAST('Native RETRY policy signal' AS INTEGER)")
    first.close()
    second.close()


def test_overlapping_adapters_on_same_connection_are_rejected_without_stale_state():
    raw = duckdb.connect()
    first, second = DuckDBAdapter(raw), DuckDBAdapter(raw)
    first.register_retry_function()
    started, release = Event(), Event()
    errors = []

    def wait_for_release():
        started.set()
        if not release.wait(10):
            raise RuntimeError("test timeout")
        return 1

    raw.create_function("transport_wait", wait_for_release, [], "INTEGER", side_effects=True)

    def execute():
        try:
            first.execute("SELECT transport_wait()")
        except Exception as exc:
            errors.append(exc)

    worker = Thread(target=execute)
    worker.start()
    try:
        assert started.wait(5)
        with pytest.raises(RuntimeError, match="Overlapping"):
            second.execute("SELECT 1")
    finally:
        release.set()
        worker.join(10)
    assert not worker.is_alive() and not errors
    assert second.execute("SELECT 1").fetchone() == (1,)
    raw.close()


def test_ordinary_udf_error_text_is_not_retry_evidence():
    raw = duckdb.connect()
    raw.execute("CREATE TABLE target(x INTEGER)")
    conn = dfc(raw)
    conn.register_policy(
        Policy(
            sources=[],
            sink="target",
            constraint="target.x > 0",
            on_fail=Resolution.RETRY,
            retry_message="trusted",
        )
    )

    def ordinary_error():
        raise ValueError("_NativeRetrySignal: Native RETRY policy signal trusted")

    raw.create_function("ordinary_error", ordinary_error, [], "INTEGER", side_effects=True)
    with pytest.raises(Exception) as caught:
        conn.execute("INSERT INTO target SELECT ordinary_error()")
    assert not isinstance(caught.value, RetryViolation)
    conn.execute("INSERT INTO target SELECT 1")
    conn.close()

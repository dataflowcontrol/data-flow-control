"""Real SQLite/DataFusion RETRY execution, not rendered-error recognition mocks."""

from __future__ import annotations

import sqlite3

import pytest
from data_flow_control import Policy, Resolution, RetryViolation, dfc
from data_flow_control.errors import _NativeRetrySignal


@pytest.fixture(params=["sqlite", "datafusion"])
def db(request):
    if request.param == "sqlite":
        raw = sqlite3.connect(":memory:")
        raw.executescript("CREATE TABLE a(x INTEGER); INSERT INTO a VALUES (1), (-1), (-2);")
    else:
        datafusion = pytest.importorskip("datafusion")
        import pyarrow as pa

        raw = datafusion.SessionContext()
        raw.register_record_batches("a", [pa.table({"x": [1, -1, -2]}).to_batches()])
    connection = dfc(raw)
    yield connection
    connection.close()


def register(db):
    for constraint, message in [
        ("a.x < 10", "passing"),
        ("a.x > 0", "Don't leak 雪\n"),
        ("a.x > -2", "second"),
    ]:
        db.register_policy(
            Policy(
                sources=["a"],
                constraint=constraint,
                on_fail=Resolution.RETRY,
                retry_message=message,
            )
        )


def test_exact_all_failures_and_next_execution(db):
    register(db)
    with pytest.raises(RetryViolation) as exc:
        db.execute("SELECT x FROM a ORDER BY x")
    assert exc.value.messages == ["Don't leak 雪\n", "second"]
    assert [f.policy_index for f in exc.value.failures] == [1, 2]
    assert db.fetchall("SELECT x FROM a WHERE x > 0") == [(1,)]
    with pytest.raises(Exception) as ordinary:
        db.execute("SELECT nonexistent FROM a")
    assert not isinstance(ordinary.value, RetryViolation)


def test_callback_transport_reset_and_shared_raw_lock(db):
    adapter = db.adapter
    with pytest.raises(_NativeRetrySignal) as exc:
        adapter.execute("SELECT passant_retry_many('2,1,2')")
    assert exc.value.failures == ((1, None), (2, None))
    with pytest.raises(Exception) as malformed:
        adapter.execute("SELECT passant_retry_many('bogus')")
    assert not isinstance(malformed.value, _NativeRetrySignal)
    assert adapter.execute("SELECT 1").fetchall() == [(1,)]
    other = type(adapter)(db.raw_connection)
    other.register_retry_function()
    assert other._retry_state is adapter._retry_state
    with adapter._retry_state.execution(), pytest.raises(RuntimeError, match="Overlapping"):
        other.execute("SELECT 1")
    assert other.execute("SELECT 1").fetchall() == [(1,)]


def test_independent_connections_do_not_share_signal(db):
    if db.adapter.dialect == "sqlite":
        raw = sqlite3.connect(":memory:")
    else:
        import datafusion

        raw = datafusion.SessionContext()
    other = dfc(raw)
    try:
        with db.adapter._retry_state.execution():
            assert other.fetchall("SELECT 1") == [(1,)]
        assert other.adapter._retry_state is not db.adapter._retry_state
    finally:
        other.close()


def test_sqlite_atomic_multirow_insert_no_rollback_of_prior_work():
    raw = sqlite3.connect(":memory:")
    raw.executescript(
        "CREATE TABLE a(x INT); INSERT INTO a VALUES (1),(-1); CREATE TABLE target(x INT);"
    )
    db = dfc(raw)
    raw.execute("INSERT INTO target VALUES(99)")
    db.register_policy(
        Policy(
            sources=["a"],
            sink="target",
            constraint="a.x>0",
            on_fail=Resolution.RETRY,
            retry_message="reject",
        )
    )
    submissions = []
    raw.set_trace_callback(submissions.append)
    with pytest.raises(RetryViolation, match="reject"):
        db.execute("INSERT INTO target SELECT x FROM a")
    assert raw.execute("SELECT x FROM target").fetchall() == [(99,)]
    assert raw.in_transaction
    assert len([s for s in submissions if "INSERT INTO target" in s]) == 1
    db.close()


def test_sqlite_late_fetch_failure_is_raised_before_any_rows_return():
    from data_flow_control.adapters.sqlite import SQLiteAdapter

    raw = sqlite3.connect(":memory:")
    adapter = SQLiteAdapter(raw)
    adapter.register_retry_function()
    with pytest.raises(_NativeRetrySignal):
        adapter.execute("SELECT 1 UNION ALL SELECT passant_retry_many('0')")
    assert adapter.execute("SELECT 1 UNION ALL SELECT 2").fetchall() == [(1,), (2,)]
    raw.close()


def test_sqlite_buffered_cursor_keeps_fetch_position_and_metadata():
    db = dfc(sqlite3.connect(":memory:"))
    cursor = db.execute("SELECT 1 AS x UNION ALL SELECT 2 UNION ALL SELECT 3")
    assert cursor.description[0][0] == "x"
    assert cursor.fetchone() == (1,)
    assert cursor.fetchmany(1) == [(2,)]
    assert list(cursor) == [(3,)]
    assert cursor.fetchone() is None
    db.close()


def test_sqlite_buffered_cursor_iteration_shares_fetch_position():
    db = dfc(sqlite3.connect(":memory:"))
    cursor = db.execute("SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3")
    assert iter(cursor) is cursor
    assert next(cursor) == (1,)
    assert cursor.fetchone() == (2,)
    assert list(cursor) == [(3,)]
    with pytest.raises(StopIteration):
        next(cursor)
    db.close()


@pytest.mark.parametrize("size", [0, -1, -2, 1, 3, None, 1.5])
def test_sqlite_buffered_fetchmany_matches_native(size):
    raw = sqlite3.connect(":memory:")
    db = dfc(raw)
    sql = "SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3"
    native, buffered = raw.execute(sql), db.execute(sql)
    assert native.fetchone() == buffered.fetchone()
    try:
        expected = native.fetchmany(size)
    except (TypeError, ValueError) as exc:
        with pytest.raises(type(exc)):
            buffered.fetchmany(size)
    else:
        assert buffered.fetchmany(size) == expected
    assert buffered.fetchall() == native.fetchall()
    db.close()


@pytest.mark.parametrize("close_connection", [False, True])
@pytest.mark.parametrize("fetch", ["fetchone", "fetchall", "fetchmany", "next"])
def test_sqlite_buffered_cursor_closed_lifetime(close_connection, fetch):
    db = dfc(sqlite3.connect(":memory:"))
    cursor = db.execute("SELECT 1 UNION ALL SELECT 2")
    if close_connection:
        db.close()
    else:
        cursor.close()
    with pytest.raises(sqlite3.ProgrammingError, match="closed"):
        next(cursor) if fetch == "next" else getattr(cursor, fetch)()
    if not close_connection:
        db.close()


def test_sqlite_buffered_cursor_fetchmany_arraysize_and_keyword():
    db = dfc(sqlite3.connect(":memory:"))
    cursor = db.execute("SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3")
    cursor.arraysize = 2
    assert cursor.fetchmany() == [(1,), (2,)]
    assert cursor.fetchmany(size=1) == [(3,)]
    db.close()


@pytest.mark.parametrize("version", [(3, 34, 1), (3, 43, 2)])
def test_sqlite_retry_version_gate_simulated_old_engine(monkeypatch, version):
    """Capability control only; this does not run an older SQLite library."""
    version_text = ".".join(map(str, version))
    monkeypatch.setattr(sqlite3, "sqlite_version_info", version)
    monkeypatch.setattr(sqlite3, "sqlite_version", version_text)
    db = dfc(sqlite3.connect(":memory:"))
    try:
        assert not db.adapter.capabilities.retry_resolution
        assert db.adapter.capabilities.exception_udf
        assert "3.44.0" in db.adapter.retry_unavailable_reason
        assert version_text in db.adapter.retry_unavailable_reason
        with pytest.raises(ValueError, match="3.44.0"):
            db.adapter.register_retry_function()
        with pytest.raises(ValueError, match="retry_resolution"):
            db.register_policy(
                Policy(
                    sources=["a"], constraint="a.x>0", on_fail=Resolution.RETRY, retry_message="bad"
                )
            )
        assert db.fetchall("SELECT 1") == [(1,)]
    finally:
        db.close()


def test_sqlite_retry_version_gate_simulated_minimum(monkeypatch):
    """Check the boundary metadata, not a claim of a real SQLite 3.44 run."""
    monkeypatch.setattr(sqlite3, "sqlite_version_info", (3, 44, 0))
    monkeypatch.setattr(sqlite3, "sqlite_version", "3.44.0")
    db = dfc(sqlite3.connect(":memory:"))
    try:
        assert db.adapter.capabilities.retry_resolution
        assert db.adapter.retry_unavailable_reason is None
    finally:
        db.close()


@pytest.mark.parametrize("use_arraysize", [False, True])
def test_sqlite_buffered_zero_fetch_preserves_native_position(use_arraysize):
    raw = sqlite3.connect(":memory:")
    db = dfc(raw)
    sql = "SELECT 1 UNION ALL SELECT 2"
    native, buffered = raw.execute(sql), db.execute(sql)
    if use_arraysize:
        native.arraysize = buffered.arraysize = 0
        assert buffered.fetchmany() == native.fetchmany()
    else:
        assert buffered.fetchmany(size=0) == native.fetchmany(size=0)
    assert buffered.fetchone() == native.fetchone()
    assert buffered.fetchall() == native.fetchall()
    db.close()
